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
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_SHIFT};
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
    /// Signalled on WM_POWERBROADCAST resume so the IP poll task can re-check
    /// immediately instead of showing stale post-sleep state for up to a poll
    /// interval. Also notified by the monitor thread on proxy state flips.
    pub ip_check_notify: Arc<Notify>,
    /// Geo cache. Consulted by the IP poll task before going to network so a
    /// previously-seen IP keeps its city without re-querying.
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
/// 拖动结束后，距工作区任一边小于该像素数时自动贴边对齐。
/// 12 是 Win11 自带 Snap Assist 触发的近似阈值，体感"挨近"和"明确贴边"
/// 之间的甜区。
const SNAP_THRESHOLD: i32 = 12;
/// snap_and_clamp 时，距工作区边缘至少留这么多像素，避免完全贴到边角
/// 把圆角和阴影裁没。
const EDGE_MARGIN: i32 = 0;
/// 全局热键 Ctrl+Alt+方向键 单次位移像素数。20px 是手感折中：低于 10px
/// 几乎看不出动，高于 30px 微调太粗。
const NUDGE_STEP: i32 = 20;

// WM_POWERBROADCAST event codes (from winuser.h). Not all of these are
// surfaced by the windows crate at the constant level we want, so we declare
// them locally to keep the dependency minimal.
const WM_POWERBROADCAST_LOCAL: u32 = 0x0218;
const PBT_APMRESUMESUSPEND: usize = 0x0007;
const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;

/// Save current window rect + lock state to the persistence file. Called on
/// drag end and on every tray-menu toggle that changes the lock state.
/// 签名收 `&RuntimeFlags` 而非 `&WindowContext`，方便设置对话框等外部
/// 调用方（它们只持有 RuntimeFlags + overlay_hwnd，没有 ctx）。
pub(crate) unsafe fn persist_overlay_state(hwnd: HWND, flags: &RuntimeFlags) {
    let mut rect = RECT::default();
    let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect);
    overlay_state::save(&OverlayPersistedState {
        x: Some(rect.left),
        y: Some(rect.top),
        locked: flags.overlay_locked.load(Ordering::Relaxed),
    });
}

/// 联动 helper：开启 click_through 自动锁定；关闭锁定自动关 click_through。
/// 多处调用（托盘菜单、设置对话框 apply），抽出来避免逻辑漂移。
/// 调用方应在调用前自己已经更新对应的"主"原子量（toggle 的那一个），
/// 这里只处理"被联动"的另一个 + 副作用（apply_click_through / persist）。
unsafe fn apply_passthrough_lock_link(
    hwnd: HWND,
    flags: &RuntimeFlags,
    new_click_through: bool,
    new_locked: bool,
) {
    let cur_locked = flags.overlay_locked.load(Ordering::Relaxed);
    let cur_click_through = flags.click_through.load(Ordering::Relaxed);

    // 开穿透 → 自动锁定（穿透下 NCHITTEST 都不触发，锁定状态保持
    // 一致，避免菜单显示"未锁定"但其实拖不动的迷惑）
    if new_click_through && !cur_locked {
        flags.overlay_locked.store(true, Ordering::Relaxed);
    }
    // 关锁定 → 自动关穿透（用户既然想拖了，免去再去托盘点一次穿透）
    if !new_locked && cur_click_through {
        flags.click_through.store(false, Ordering::Relaxed);
        apply_click_through(hwnd, false);
    }
}

/// 切换主浮窗的 WS_EX_TRANSPARENT 扩展样式，运行时实现"鼠标穿透"开关
/// 而无需销毁/重建窗口。设置对话框 / 托盘菜单都走这条。
pub(crate) unsafe fn set_overlay_click_through(hwnd: HWND, enable: bool) {
    apply_click_through(hwnd, enable);
}

/// 全局热键 Ctrl+Alt+方向键的统一处理：平移浮窗 (dx, dy)，做边缘 clamp，
/// 关 auto_center，写盘。锁定状态也走这条 —— 微调是显式动作，比拖动更
/// 难误触，没必要因锁定就禁掉。
unsafe fn nudge_overlay(hwnd: HWND, dx: i32, dy: i32, ctx_ptr: *const WindowContext) {
    if ctx_ptr.is_null() {
        return;
    }
    let ctx = &*ctx_ptr;
    let mut r = RECT::default();
    if GetWindowRect(hwnd, &mut r).is_err() {
        return;
    }
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        r.left + dx,
        r.top + dy,
        0,
        0,
        SWP_NOSIZE | SWP_NOACTIVATE,
    );
    // 不吸附（snap=false）—— 微调本身就是用户在 ±20px 粒度抠位置，
    // 不该再被 12px 吸附跳走。但仍要 clamp 防出屏。
    snap_and_clamp(hwnd, false);
    ctx.auto_center.store(false, Ordering::Relaxed);
    persist_overlay_state(hwnd, &ctx.runtime_flags);
}

/// 是否按住 Shift。用于锁定模式下的"按 Shift 临时拖动"——锁定开了之后
/// 用户偶尔想精调位置，不必先去托盘解锁。
unsafe fn shift_held() -> bool {
    // GetAsyncKeyState 返回 i16，高位 set 表示当前按下。这里不依赖
    // 消息队列，纯粹查询硬件状态，对 hit-test 这种高频回调安全。
    (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0
}

/// 把窗口位置规整到所在显示器的工作区内：
/// 1. 若窗口完全落在所有显示器之外（多显示器拓扑变化、分辨率切换后启动
///    恢复位置时常见），夹回最近显示器的工作区
/// 2. 否则取窗口当前所在显示器的工作区，做边缘吸附 + clamp
///
/// `snap` 控制是否启用吸附；启动恢复位置时只想 clamp 不想吸附（用户原本
/// 拖到的位置可能距边 13px，不该被启动后悄悄改成 0px）。
unsafe fn snap_and_clamp(hwnd: HWND, snap: bool) -> bool {
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return false;
    }
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;

    // MonitorFromWindow 默认拿最近的，即便窗口完全在屏外也会给一个 sane 的
    // monitor。这就解决了"显示器被拔了"的场景。
    let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
        return false;
    }
    let work = mi.rcWork;

    let mut x = rect.left;
    let mut y = rect.top;

    // 1) Snap：距边 < SNAP_THRESHOLD 时贴齐（带 EDGE_MARGIN 留呼吸位）
    if snap {
        if (x - work.left).abs() <= SNAP_THRESHOLD {
            x = work.left + EDGE_MARGIN;
        } else if (work.right - (x + w)).abs() <= SNAP_THRESHOLD {
            x = work.right - w - EDGE_MARGIN;
        }
        if (y - work.top).abs() <= SNAP_THRESHOLD {
            y = work.top + EDGE_MARGIN;
        } else if (work.bottom - (y + h)).abs() <= SNAP_THRESHOLD {
            y = work.bottom - h - EDGE_MARGIN;
        }
    }

    // 2) Clamp：永远把窗口完整夹回工作区，防止任何角落跑出去
    //    （窗口比工作区还宽的极端情况，优先保左/上可见）
    let max_x = (work.right - w).max(work.left);
    let max_y = (work.bottom - h).max(work.top);
    x = x.clamp(work.left, max_x);
    y = y.clamp(work.top, max_y);

    if x != rect.left || y != rect.top {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
        true
    } else {
        false
    }
}

/// 立刻重新测量并应用宽度+高度。形态/模式切换走这个，避免等下次 channel 消息。
/// 高度按当前 form 决定（simple=2 行，detailed=2 行+sparkline 行）。
unsafe fn recalc_overlay_width(hwnd: HWND, ctx: &WindowContext) {
    let (required, height) = {
        let s = match ctx.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let hdc = GetDC(Some(hwnd));
        let w = render::measure_required_width(hdc, &s);
        ReleaseDC(Some(hwnd), hdc);
        (w, render::WIN_HEIGHT)
    };
    let flags = SWP_NOACTIVATE
        | if ctx.auto_center.load(Ordering::Relaxed) {
            SET_WINDOW_POS_FLAGS(0)
        } else {
            SWP_NOMOVE
        };
    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, required, height, flags);
    let _ = InvalidateRect(Some(hwnd), None, true);
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
        let failed_hotkeys = hotkey::register_hotkeys(hwnd, config);
        if !failed_hotkeys.is_empty() {
            // 关键诊断点：用户经常以为"app 坏了"，其实是 Ctrl+Alt+I 之类
            // 的组合被 IDE 抢了。弹一次 MessageBox 比埋日志直观得多。
            // 用 MB_TASKMODAL 避免 owner 缺失警告；MB_ICONWARNING 表明
            // 非致命。
            let lines: Vec<String> = failed_hotkeys
                .iter()
                .map(|(name, combo)| format!("  • {} ({})", name, combo))
                .collect();
            let body = format!(
                "以下全局热键注册失败，可能已被其他程序占用：\n\n{}\n\n\
                 程序仍可正常运行，请通过托盘图标操作；\n\
                 或打开 \"高级设置...\" → \"热键\" 修改键位组合。",
                lines.join("\n")
            );
            let body_w: Vec<u16> = body.encode_utf16().chain(std::iter::once(0)).collect();
            let title_w: Vec<u16> = "Vpn Monitor — 热键冲突".encode_utf16().chain(std::iter::once(0)).collect();
            MessageBoxW(
                None,
                windows::core::PCWSTR(body_w.as_ptr()),
                windows::core::PCWSTR(title_w.as_ptr()),
                MB_OK | MB_ICONWARNING | MB_TASKMODAL | MB_TOPMOST,
            );
        }

        // Register tray icon. If this fails (Explorer not running, etc.) the
        // overlay still functions — only the right-click menu becomes
        // unavailable.
        tray::register(hwnd);

        // Periodic timer to re-assert topmost — full-screen apps / UAC dialogs
        // can push us down even though WS_EX_TOPMOST is set.
        SetTimer(Some(hwnd), TIMER_ID_TOPMOST, TOPMOST_REFRESH_MS, None);

        // Restore last known position if persisted. Otherwise the centering
        // path in compute_window_origin kicks in on the first repaint.
        // 启动后立即做一次 clamp（不吸附），处理"上次记的坐标在已拔掉的
        // 副显示器上"这种情况——否则浮窗会停在用户根本看不到的地方。
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
            snap_and_clamp(hwnd, false);
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
                    let height = render::WIN_HEIGHT;
                    if ctx_loop.auto_center.load(Ordering::Relaxed) {
                        let (x, y) = compute_window_origin(hwnd, required);
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            x,
                            y,
                            required,
                            height,
                            SWP_NOACTIVATE,
                        );
                    } else {
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            0,
                            0,
                            required,
                            height,
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
            //
            // 锁定状态下默认拒绝拖动，但若用户按住 Shift，仍允许 OS 接管，
            // 实现"按 Shift 临时拖动"的微调手势。免去先去托盘解锁再锁回的
            // 三步操作。
            let ctx_ptr =
                GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *const WindowContext;
            if !ctx_ptr.is_null() {
                let locked = (&(*ctx_ptr).runtime_flags)
                    .overlay_locked
                    .load(Ordering::Relaxed);
                if !locked || shift_held() {
                    return LRESULT(HTCAPTION as isize);
                }
            }
            DefWindowProcA(hwnd, msg, wparam, lparam)
        }
        WM_EXITSIZEMOVE => {
            // 用户结束拖动 → 关掉 auto_center（之后 width 变化只改宽不
            // 重置位置），先做边缘吸附 + 屏外约束，再把规整后的新位置
            // 写盘。snap_and_clamp 内部会 SetWindowPos，写盘读到的就是
            // 最终坐标，不会落差 12px。
            let ctx_ptr =
                GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *const WindowContext;
            if !ctx_ptr.is_null() {
                (*ctx_ptr).auto_center.store(false, Ordering::Relaxed);
                snap_and_clamp(hwnd, true);
                persist_overlay_state(hwnd, &(*ctx_ptr).runtime_flags);
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
                        let new_locked = RuntimeFlags::toggle(&ctx.runtime_flags.overlay_locked);
                        // 联动：关闭锁定 → 自动关穿透（拖动前提：穿透必须关）
                        apply_passthrough_lock_link(
                            hwnd,
                            &ctx.runtime_flags,
                            ctx.runtime_flags.click_through.load(Ordering::Relaxed),
                            new_locked,
                        );
                        persist_overlay_state(hwnd, &ctx.runtime_flags);
                    }
                    tray::IDM_RESET_POSITION => {
                        // 一键回归"开局形态"：重新打开 auto_center；按当前
                        // 宽度计算目标显示器顶部居中坐标；立刻 SetWindowPos
                        // 过去；持久化新位置（也覆盖掉 overlay_state.json
                        // 里旧的离屏坐标，下次启动直接 ok）。
                        ctx.auto_center.store(true, Ordering::Relaxed);
                        let mut cur = RECT::default();
                        let _ = GetWindowRect(hwnd, &mut cur);
                        let w = (cur.right - cur.left).max(1);
                        let height = render::WIN_HEIGHT;
                        let (x, y) = compute_window_origin(hwnd, w);
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            x,
                            y,
                            w,
                            height,
                            SWP_NOACTIVATE,
                        );
                        persist_overlay_state(hwnd, &ctx.runtime_flags);
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                    tray::IDM_TOGGLE_CLICKTHROUGH => {
                        let new = RuntimeFlags::toggle(&ctx.runtime_flags.click_through);
                        apply_click_through(hwnd, new);
                        // 联动：开穿透 → 自动锁定（避免 NCHITTEST 拿不到的迷惑）
                        apply_passthrough_lock_link(
                            hwnd,
                            &ctx.runtime_flags,
                            new,
                            ctx.runtime_flags.overlay_locked.load(Ordering::Relaxed),
                        );
                        // 联动可能改了 overlay_locked，写盘一次同步到磁盘
                        persist_overlay_state(hwnd, &ctx.runtime_flags);
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
                    tray::IDM_HISTORY => {
                        // 历史时间线窗口 —— 独立线程跑自己的消息泵
                        let cache = ctx.geo_cache.clone();
                        let parent_raw = hwnd.0 as usize;
                        std::thread::Builder::new()
                            .name("vpn-monitor-history".into())
                            .spawn(move || {
                                let parent = HWND(parent_raw as *mut _);
                                let mut dlg =
                                    super::history_dialog::HistoryDialog::new(cache);
                                dlg.show(parent);
                            })
                            .ok();
                    }
                    tray::IDM_USAGE_DETAIL => {
                        // AI 用量明细窗口
                        let parent_raw = hwnd.0 as usize;
                        std::thread::Builder::new()
                            .name("vpn-monitor-usage".into())
                            .spawn(move || {
                                let parent = HWND(parent_raw as *mut _);
                                let mut dlg = super::usage_dialog::UsageDialog::new();
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
                    tray::IDM_ROW2_SYSTEM | tray::IDM_ROW2_USAGE => {
                        let new_mode = if id == tray::IDM_ROW2_USAGE {
                            "usage"
                        } else {
                            "system"
                        };
                        if let Ok(mut g) = ctx.runtime_flags.row2_mode.write() {
                            *g = new_mode.to_string();
                        }
                        if let Ok(mut s) = ctx.state.lock() {
                            s.row2_mode = new_mode.to_string();
                        }
                        // row2 内容不同宽，也重算
                        recalc_overlay_width(hwnd, ctx);
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
                hotkey::HOTKEY_QUIT => {
                    // Triggering DestroyWindow funnels through WM_DESTROY →
                    // PostQuitMessage → WM_QUIT, which gives WM_NCDESTROY a
                    // chance to reclaim the ctx Arc and Windows a chance to
                    // unregister hotkeys cleanly.
                    let _ = DestroyWindow(hwnd);
                }
                hotkey::HOTKEY_NUDGE_UP => nudge_overlay(hwnd, 0, -NUDGE_STEP, ctx_ptr),
                hotkey::HOTKEY_NUDGE_DOWN => nudge_overlay(hwnd, 0, NUDGE_STEP, ctx_ptr),
                hotkey::HOTKEY_NUDGE_LEFT => nudge_overlay(hwnd, -NUDGE_STEP, 0, ctx_ptr),
                hotkey::HOTKEY_NUDGE_RIGHT => nudge_overlay(hwnd, NUDGE_STEP, 0, ctx_ptr),
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
                persist_overlay_state(hwnd, &(*ctx_ptr).runtime_flags);
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
