use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, Notify};
use windows::core::s;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::AppConfig;
use crate::monitor::MonitorSample;

use crate::network::geo_cache::GeoCache;
use crate::runtime::RuntimeFlags;

use super::hotkey;
use super::overlay_state::{self, OverlayPersistedState};
use super::render::{self, IpUpdate, SharedState, WIN_HEIGHT};
use super::tray;

pub enum UiUpdate {
    Ip(IpUpdate),
    Monitor(MonitorSample),
}

pub(crate) struct WindowContext {
    pub state: SharedState,
    pub client: reqwest::Client,
    pub lookup_dialog_open: AtomicBool,
    /// Signalled on WM_POWERBROADCAST resume so the IP poll task can re-check
    /// immediately instead of showing stale post-sleep state for up to a poll
    /// interval. Also notified by the monitor thread on proxy state flips.
    pub ip_check_notify: Arc<Notify>,
    /// Geo cache. The lookup dialog consults it before going to network so a
    /// previously-seen IP shows its city instantly.
    pub geo_cache: Option<Arc<GeoCache>>,
    /// Runtime-toggleable flags shared with the IP poll task & monitor — the
    /// tray menu flips bits here and the next tick picks them up.
    pub runtime_flags: Arc<RuntimeFlags>,
    /// When true, width-change repaints re-center the overlay on its monitor.
    /// Becomes false the first time the user drags the window, so subsequent
    /// resizes keep the user-chosen position. Persisted positions also flip
    /// this off on startup.
    pub auto_center: AtomicBool,
}

const WIN_Y_OFFSET: i32 = 8;
const TIMER_ID_TOPMOST: usize = 1;
const TOPMOST_REFRESH_MS: u32 = 3000;
const WAIT_TIMEOUT_MS: u32 = 16;

// WM_POWERBROADCAST event codes (from winuser.h). Not all of these are
// surfaced by the windows crate at the constant level we want, so we declare
// them locally to keep the dependency minimal.
const WM_POWERBROADCAST_LOCAL: u32 = 0x0218;
const PBT_APMRESUMESUSPEND: usize = 0x0007;
const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;

/// Spawn a worker thread + lookup dialog. Shared between the hotkey and the
/// tray menu so neither path drifts.
unsafe fn open_lookup_dialog(parent_hwnd: HWND, ctx_ptr: *const WindowContext) {
    if ctx_ptr.is_null() {
        return;
    }
    let ctx = &*ctx_ptr;
    if ctx
        .lookup_dialog_open
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let ctx_clone = clone_ctx_arc(ctx_ptr);
    let client = ctx_clone.client.clone();
    let cache = ctx_clone.geo_cache.clone();
    let parent_hwnd_raw = parent_hwnd.0 as usize;
    std::thread::Builder::new()
        .name("vpn-monitor-lookup".into())
        .spawn(move || {
            let parent_hwnd = HWND(parent_hwnd_raw as *mut _);
            let mut dialog = super::lookup_dialog::LookupDialog::new(client, cache);
            dialog.show(parent_hwnd);
            ctx_clone
                .lookup_dialog_open
                .store(false, Ordering::SeqCst);
        })
        .ok();
}

/// Save current window rect + lock state to the persistence file. Called on
/// drag end and on every tray-menu toggle that changes the lock state.
unsafe fn persist_overlay_state(hwnd: HWND, ctx: &WindowContext) {
    let mut rect = RECT::default();
    let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect);
    overlay_state::save(&OverlayPersistedState {
        x: Some(rect.left),
        y: Some(rect.top),
        locked: ctx.runtime_flags.overlay_locked.load(Ordering::Relaxed),
    });
}

/// 切换主浮窗的 WS_EX_TRANSPARENT 扩展样式，运行时实现"鼠标穿透"开关
/// 而无需销毁/重建窗口。设置对话框 / 托盘菜单都走这条。
pub(crate) unsafe fn set_overlay_click_through(hwnd: HWND, enable: bool) {
    apply_click_through(hwnd, enable);
}

/// 跨线程改主浮窗左上的 claude_model 文本。设置对话框切 cc-switch 源
/// 后调它立刻看到新 tag。
/// 实现：把 String 装箱进 raw ptr，PostMessage 给主线程，主线程在
/// WM_APP_SET_LABEL 分支取出来写 state + InvalidateRect。
pub fn set_overlay_claude_label(hwnd: HWND, label: String) {
    let boxed = Box::into_raw(Box::new(label));
    unsafe {
        let _ = PostMessageW(
            Some(hwnd),
            WM_APP_SET_LABEL,
            WPARAM(boxed as usize),
            LPARAM(0),
        );
    }
}

pub const WM_APP_SET_LABEL: u32 = WM_APP + 3;
pub const WM_APP_THEME_CHANGED: u32 = WM_APP + 4;

/// 跨线程通知主浮窗刷新主题色（设置对话框改 theme 字段时调）。
/// 主线程在 WM_APP_THEME_CHANGED 分支重读 runtime_flags.theme_mode →
/// 调 theme::resolve → 写 state.theme → InvalidateRect。
pub fn notify_theme_changed(hwnd: HWND) {
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP_THEME_CHANGED, WPARAM(0), LPARAM(0));
    }
}

unsafe fn apply_click_through(hwnd: HWND, enable: bool) {
    let cur = GetWindowLongPtrA(hwnd, GWL_EXSTYLE);
    let new = if enable {
        cur | (WS_EX_TRANSPARENT.0 as isize)
    } else {
        cur & !(WS_EX_TRANSPARENT.0 as isize)
    };
    SetWindowLongPtrA(hwnd, GWL_EXSTYLE, new);
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
    );
}

/// SAFETY: Caller must ensure the pointer came from `Arc::into_raw` on a
/// matching `Arc<WindowContext>` that is still alive. Returns an owning Arc
/// clone (strong count is incremented).
unsafe fn clone_ctx_arc(ptr: *const WindowContext) -> Arc<WindowContext> {
    Arc::increment_strong_count(ptr);
    Arc::from_raw(ptr)
}

/// Lock state, recovering from poison so a panic in one critical section
/// doesn't permanently kill the UI.
fn lock_state<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poison) => poison.into_inner(),
    }
}

pub fn create_and_run(
    config: &AppConfig,
    state: SharedState,
    mut rx: mpsc::UnboundedReceiver<UiUpdate>,
    client: reqwest::Client,
    ip_check_notify: Arc<Notify>,
    geo_cache: Option<Arc<GeoCache>>,
    runtime_flags: Arc<RuntimeFlags>,
    persisted: OverlayPersistedState,
) {
    unsafe {
        let hmodule = GetModuleHandleA(None).unwrap_or_default();
        let hinstance: HINSTANCE = hmodule.into();
        let class_name = s!("VpnMonitorOverlay");

        let bg_brush = CreateSolidBrush(render::BG_COLOR);

        // 嵌入图标（资源 ID 1），失败回退默认 —— alt-tab / 任务栏会用它。
        let app_icon = LoadIconW(
            Some(hinstance),
            windows::core::PCWSTR(1 as *const _),
        )
        .unwrap_or_default();

        let wc = WNDCLASSA {
            hInstance: hinstance,
            lpszClassName: class_name,
            lpfnWndProc: Some(window_proc),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: bg_brush,
            hIcon: app_icon,
            ..Default::default()
        };

        RegisterClassA(&wc);

        let mut ex_style = WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW;
        if config.click_through {
            ex_style |= WS_EX_TRANSPARENT;
        }

        // Wrap context in Arc; pass an Arc-owned raw pointer as lpCreateParams.
        // The window holds one strong reference; spawned threads can hold their
        // own clones. WM_NCDESTROY reclaims the window's reference.
        let had_persisted_pos = persisted.x.is_some() && persisted.y.is_some();
        let ctx = Arc::new(WindowContext {
            state: state.clone(),
            client: client.clone(),
            lookup_dialog_open: AtomicBool::new(false),
            ip_check_notify,
            geo_cache,
            runtime_flags: runtime_flags.clone(),
            auto_center: AtomicBool::new(!had_persisted_pos),
        });
        // Keep a clone for the main loop's direct reads of ctx fields; the
        // raw pointer is what GWLP_USERDATA stores.
        let ctx_loop = ctx.clone();
        let ctx_raw: *const WindowContext = Arc::into_raw(ctx);

        let hwnd = match CreateWindowExA(
            ex_style,
            class_name,
            s!(""),
            WS_POPUP | WS_VISIBLE,
            0,
            WIN_Y_OFFSET,
            1,
            WIN_HEIGHT,
            None,
            None,
            Some(hinstance),
            Some(ctx_raw as *mut _),
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("Failed to create overlay window: {:?}", e);
                // Reclaim the Arc we leaked above so it can be dropped.
                let _ = Arc::from_raw(ctx_raw);
                return;
            }
        };

        let pref = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const _,
            std::mem::size_of_val(&pref) as u32,
        );

        render::set_window_opacity(hwnd, config.opacity);
        hotkey::register_hotkeys(hwnd, config);

        // Register tray icon. If this fails (Explorer not running, etc.) the
        // overlay still functions — only the right-click menu becomes
        // unavailable.
        tray::register(hwnd);

        // Periodic timer to re-assert topmost — full-screen apps / UAC dialogs
        // can push us down even though WS_EX_TOPMOST is set.
        SetTimer(Some(hwnd), TIMER_ID_TOPMOST, TOPMOST_REFRESH_MS, None);

        // Restore last known position if persisted. Otherwise the centering
        // path in compute_window_origin kicks in on the first repaint.
        if let (Some(x), Some(y)) = (persisted.x, persisted.y) {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }

        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = UpdateWindow(hwnd);

        // Initial state
        {
            let mut s = lock_state(&state);
            s.current_update = IpUpdate {
                ip: None,
                geo: None,
                status: render::CheckStatus::Checking,
                latency_ms: None,
                error_reason: None,
                geo_error_reason: None,
                geo_warning: None,
            };
        }

        let mut current_width: i32 = 1;

        // Main message loop. Use MsgWaitForMultipleObjectsEx so we wake
        // immediately on input rather than burning CPU with a fixed sleep,
        // but still time-bound the wait so we can poll the mpsc channel.
        loop {
            // 1. Drain channel
            let mut need_repaint = false;
            loop {
                match rx.try_recv() {
                    Ok(UiUpdate::Ip(update)) => {
                        tracing::info!(
                            "[UI] 收到IP更新: ip={:?}, status={:?}",
                            update.ip, update.status
                        );
                        lock_state(&state).current_update = update;
                        need_repaint = true;
                    }
                    Ok(UiUpdate::Monitor(sample)) => {
                        let mut s = lock_state(&state);
                        s.cpu_usage = sample.cpu_usage;
                        s.mem_usage = sample.mem_usage;
                        s.net_up = sample.net_upload_bps;
                        s.net_down = sample.net_download_bps;
                        s.proxy_enabled = sample.proxy_enabled;
                        drop(s);
                        need_repaint = true;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                }
            }

            if need_repaint {
                let required = {
                    let s = lock_state(&state);
                    let hdc = GetDC(Some(hwnd));
                    let w = render::measure_required_width(hdc, &s);
                    ReleaseDC(Some(hwnd), hdc);
                    w
                };

                if required != current_width {
                    current_width = required;
                    // Re-center on resize only if the user hasn't manually
                    // positioned the window. If they have, just change width
                    // and leave the X/Y alone via SWP_NOMOVE.
                    if ctx_loop.auto_center.load(Ordering::Relaxed) {
                        let (x, y) = compute_window_origin(hwnd, required);
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            x,
                            y,
                            required,
                            WIN_HEIGHT,
                            SWP_NOACTIVATE,
                        );
                    } else {
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            0,
                            0,
                            required,
                            WIN_HEIGHT,
                            SWP_NOMOVE | SWP_NOACTIVATE,
                        );
                    }
                }

                let _ = InvalidateRect(Some(hwnd), None, true);
            }

            // 2. Wait for OS messages or short timeout
            let _ = MsgWaitForMultipleObjectsEx(
                None,
                WAIT_TIMEOUT_MS,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );

            // 3. Pump messages
            let mut quit = false;
            let mut msg = MSG::default();
            while PeekMessageA(&mut msg, None, 0, 0, PM_REMOVE).into() {
                if msg.message == WM_QUIT {
                    quit = true;
                    break;
                }
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageA(&msg);
            }
            if quit {
                break;
            }
        }
        // WM_NCDESTROY (via DestroyWindow → WM_DESTROY → PostQuitMessage path)
        // already reclaimed the ctx Arc and unregistered hotkeys. If the loop
        // somehow exited without WM_DESTROY firing, force-destroy the window.
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
    }
}

unsafe fn compute_window_origin(hwnd: HWND, width: i32) -> (i32, i32) {
    // Pick the monitor the window is currently on (defaults to nearest if not
    // yet visible). Use the work area so we don't collide with the taskbar.
    let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        let left = mi.rcWork.left;
        let work_w = mi.rcWork.right - mi.rcWork.left;
        let x = left + (work_w - width) / 2;
        let y = mi.rcWork.top + WIN_Y_OFFSET;
        (x, y)
    } else {
        // Fallback to primary screen metrics
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        ((screen_w - width) / 2, WIN_Y_OFFSET)
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = lparam.0 as *const CREATESTRUCTA;
            if !cs.is_null() {
                let raw = (*cs).lpCreateParams as isize;
                SetWindowLongPtrA(hwnd, GWLP_USERDATA, raw);
            }
            DefWindowProcA(hwnd, msg, wparam, lparam)
        }
        WM_PAINT => {
            let ctx_ptr =
                GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *const WindowContext;
            if !ctx_ptr.is_null() {
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                let state = lock_state(&(*ctx_ptr).state);
                render::paint_overlay(hwnd, &state, rect.right, rect.bottom);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_TIMER => {
            if wparam.0 == TIMER_ID_TOPMOST {
                // Re-assert topmost. SWP_NOACTIVATE keeps us from stealing focus.
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // System suggests a new rect in lparam; honor its position but
            // we'll re-measure width on next channel update.
            let suggested = lparam.0 as *const RECT;
            if !suggested.is_null() {
                let r = *suggested;
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOACTIVATE,
                );
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            // Resolution / monitor topology changed — recenter on next paint.
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_NCHITTEST => {
            // 未锁定时整个浮窗都是拖动区。注意：不能依赖 DefWindowProc
            // 返回 HTCLIENT —— 我们是 WS_POPUP + WS_EX_LAYERED 无边框窗口，
            // 默认 hit-test 会返回 HTNOWHERE(0)，永远进不了"如果是 HTCLIENT
            // 就提升"的分支。所以这里无条件返回 HTCAPTION，OS 自然处理拖动。
            let ctx_ptr =
                GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *const WindowContext;
            if !ctx_ptr.is_null() {
                let locked = (&(*ctx_ptr).runtime_flags)
                    .overlay_locked
                    .load(Ordering::Relaxed);
                if !locked {
                    return LRESULT(HTCAPTION as isize);
                }
            }
            DefWindowProcA(hwnd, msg, wparam, lparam)
        }
        WM_EXITSIZEMOVE => {
            // 用户结束拖动 → 关掉 auto_center（之后 width 变化只改宽不
            // 重置位置），并立即把新位置写盘，下次启动恢复。
            let ctx_ptr =
                GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *const WindowContext;
            if !ctx_ptr.is_null() {
                (*ctx_ptr).auto_center.store(false, Ordering::Relaxed);
                persist_overlay_state(hwnd, &*ctx_ptr);
            }
            LRESULT(0)
        }
        m if m == WM_APP_THEME_CHANGED => {
            // 主题切换：读最新 mode，调 theme::resolve，写 state.theme + 重画
            let ctx_ptr =
                GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *const WindowContext;
            if !ctx_ptr.is_null() {
                let mode = (&(*ctx_ptr).runtime_flags)
                    .theme_mode
                    .read()
                    .map(|g| g.clone())
                    .unwrap_or_else(|p| p.into_inner().clone());
                let new_theme = super::theme::resolve(&mode);
                if let Ok(mut s) = (*ctx_ptr).state.lock() {
                    s.theme = new_theme;
                }
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            // 用户切了 OS 个性化（暗色 ↔ 亮色），lparam 是个 LPCTSTR
            // 指向 "ImmersiveColorSet"。我们简单粗暴：只要 mode == system
            // 就重新探测一次主题。
            let ctx_ptr =
                GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *const WindowContext;
            if !ctx_ptr.is_null() {
                let mode = (&(*ctx_ptr).runtime_flags)
                    .theme_mode
                    .read()
                    .map(|g| g.clone())
                    .unwrap_or_else(|p| p.into_inner().clone());
                if mode == "system" {
                    let new_theme = super::theme::resolve(&mode);
                    if let Ok(mut s) = (*ctx_ptr).state.lock() {
                        s.theme = new_theme;
                    }
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
            }
            DefWindowProcA(hwnd, msg, wparam, lparam)
        }
        m if m == WM_APP_SET_LABEL => {
            // 跨线程更新左上 claude_model 文本：从 boxed 指针取回 String，
            // 写入 SharedState，再 InvalidateRect 重画。
            let ptr = wparam.0 as *mut String;
            if !ptr.is_null() {
                let label = *Box::from_raw(ptr);
                let ctx_ptr =
                    GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *const WindowContext;
                if !ctx_ptr.is_null() {
                    if let Ok(mut s) = (*ctx_ptr).state.lock() {
                        s.claude_model = label;
                    }
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
            }
            LRESULT(0)
        }
        m if m == tray::WM_APP_TRAY => {
            // 托盘图标的 callback：lparam 低位字带鼠标事件类型。左右键
            // 抬起都算"召唤菜单"，比强制右键更友好（笔记本触摸板用户）。
            let event = (lparam.0 as u32) & 0xFFFF;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                let ctx_ptr =
                    GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *const WindowContext;
                if !ctx_ptr.is_null() {
                    tray::show_menu(hwnd, &(*ctx_ptr).runtime_flags);
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 as u32) & 0xFFFF;
            let ctx_ptr =
                GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *const WindowContext;
            if !ctx_ptr.is_null() {
                let ctx = &*ctx_ptr;
                match id {
                    tray::IDM_TOGGLE_VISIBLE => {
                        let new = RuntimeFlags::toggle(&ctx.runtime_flags.overlay_visible);
                        let _ = ShowWindow(
                            hwnd,
                            if new { SW_SHOWNOACTIVATE } else { SW_HIDE },
                        );
                        if let Ok(mut s) = ctx.state.lock() {
                            s.visible = new;
                        }
                    }
                    tray::IDM_TOGGLE_LOCK => {
                        RuntimeFlags::toggle(&ctx.runtime_flags.overlay_locked);
                        persist_overlay_state(hwnd, ctx);
                    }
                    tray::IDM_TOGGLE_CLICKTHROUGH => {
                        let new = RuntimeFlags::toggle(&ctx.runtime_flags.click_through);
                        apply_click_through(hwnd, new);
                    }
                    tray::IDM_TOGGLE_MASK_IP => {
                        let new = !crate::network::ip_fetcher::get_mask_ip_logs();
                        crate::network::ip_fetcher::set_mask_ip_logs(new);
                    }
                    tray::IDM_TOGGLE_MASK_GEO => {
                        let new = !crate::network::ip_fetcher::get_mask_geo_logs();
                        crate::network::ip_fetcher::set_mask_geo_logs(new);
                    }
                    tray::IDM_TOGGLE_GEO_CACHE => {
                        RuntimeFlags::toggle(&ctx.runtime_flags.geo_cache_enabled);
                    }
                    tray::IDM_TOGGLE_CROSS_CHECK => {
                        RuntimeFlags::toggle(&ctx.runtime_flags.geo_cross_check);
                    }
                    tray::IDM_LOOKUP => {
                        open_lookup_dialog(hwnd, ctx_ptr);
                    }
                    tray::IDM_HISTORY => {
                        // 历史时间线窗口 —— 独立线程跑自己的消息泵
                        let client = ctx.client.clone();
                        let cache = ctx.geo_cache.clone();
                        let parent_raw = hwnd.0 as usize;
                        std::thread::Builder::new()
                            .name("vpn-monitor-history".into())
                            .spawn(move || {
                                let parent = HWND(parent_raw as *mut _);
                                let mut dlg =
                                    super::history_dialog::HistoryDialog::new(
                                        client, cache,
                                    );
                                dlg.show(parent);
                            })
                            .ok();
                    }
                    tray::IDM_ADVANCED => {
                        // 高级设置对话框 —— 独立线程跑自己的消息泵
                        let runtime_flags = ctx.runtime_flags.clone();
                        let overlay_hwnd_raw = hwnd.0 as usize;
                        std::thread::Builder::new()
                            .name("vpn-monitor-settings".into())
                            .spawn(move || {
                                let overlay_hwnd = HWND(overlay_hwnd_raw as *mut _);
                                let mut dlg =
                                    super::settings_dialog::SettingsDialog::new(
                                        runtime_flags,
                                        overlay_hwnd,
                                    );
                                dlg.show(overlay_hwnd);
                            })
                            .ok();
                    }
                    tray::IDM_OPEN_CONFIG => {
                        tray::open_external(&crate::config::config_path());
                    }
                    tray::IDM_OPEN_LOG_DIR => {
                        if let Some(d) = dirs::data_dir() {
                            let log_dir = d.join("Vpn_Monitor");
                            tray::open_external(&log_dir);
                        }
                    }
                    tray::IDM_QUIT => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_POWERBROADCAST_LOCAL => {
            // Resume from sleep/hibernate. Signal the IP poll task so we
            // refetch immediately instead of displaying the stale pre-sleep
            // IP for up to one poll interval (often 10+ seconds).
            if wparam.0 == PBT_APMRESUMEAUTOMATIC || wparam.0 == PBT_APMRESUMESUSPEND {
                tracing::info!("System resumed from sleep, triggering IP re-check");
                let ctx_ptr =
                    GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *const WindowContext;
                if !ctx_ptr.is_null() {
                    (*ctx_ptr).ip_check_notify.notify_one();
                }
            }
            LRESULT(1)
        }
        WM_HOTKEY => {
            let hotkey_id = wparam.0 as i32;
            let ctx_ptr =
                GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *const WindowContext;

            match hotkey_id {
                hotkey::HOTKEY_TOGGLE => {
                    if !ctx_ptr.is_null() {
                        let mut s = lock_state(&(*ctx_ptr).state);
                        s.visible = !s.visible;
                        if s.visible {
                            drop(s);
                            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                            // Re-topmost on re-show
                            let _ = SetWindowPos(
                                hwnd,
                                Some(HWND_TOPMOST),
                                0,
                                0,
                                0,
                                0,
                                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                            );
                        } else {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                        }
                    }
                }
                hotkey::HOTKEY_LOOKUP => {
                    open_lookup_dialog(hwnd, ctx_ptr);
                }
                hotkey::HOTKEY_QUIT => {
                    // Triggering DestroyWindow funnels through WM_DESTROY →
                    // PostQuitMessage → WM_QUIT, which gives WM_NCDESTROY a
                    // chance to reclaim the ctx Arc and Windows a chance to
                    // unregister hotkeys cleanly.
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // Persist the final position/lock state on the way out so a clean
            // exit after a drag still remembers where the user left us.
            let ctx_ptr =
                GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *const WindowContext;
            if !ctx_ptr.is_null() {
                persist_overlay_state(hwnd, &*ctx_ptr);
            }
            tray::unregister(hwnd);
            hotkey::unregister_hotkeys(hwnd);
            KillTimer(Some(hwnd), TIMER_ID_TOPMOST).ok();
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // Reclaim the Arc reference the window held. If any worker thread
            // still holds a clone (e.g. a lookup dialog still up), the Arc
            // stays alive until that thread drops its clone.
            let raw = GetWindowLongPtrA(hwnd, GWLP_USERDATA);
            if raw != 0 {
                let arc = Arc::from_raw(raw as *const WindowContext);
                drop(arc);
                SetWindowLongPtrA(hwnd, GWLP_USERDATA, 0);
            }
            DefWindowProcA(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcA(hwnd, msg, wparam, lparam),
    }
}
