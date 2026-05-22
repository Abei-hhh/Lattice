use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
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

use super::hotkey;
use super::render::{self, IpUpdate, SharedState, WIN_HEIGHT};

pub enum UiUpdate {
    Ip(IpUpdate),
    Monitor(MonitorSample),
}

pub(crate) struct WindowContext {
    pub state: SharedState,
    pub client: reqwest::Client,
    pub lookup_dialog_open: AtomicBool,
}

const WIN_Y_OFFSET: i32 = 8;
const TIMER_ID_TOPMOST: usize = 1;
const TOPMOST_REFRESH_MS: u32 = 3000;
const WAIT_TIMEOUT_MS: u32 = 16;

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
) {
    unsafe {
        let hmodule = GetModuleHandleA(None).unwrap_or_default();
        let hinstance: HINSTANCE = hmodule.into();
        let class_name = s!("VpnMonitorOverlay");

        let bg_brush = CreateSolidBrush(render::BG_COLOR);

        let wc = WNDCLASSA {
            hInstance: hinstance,
            lpszClassName: class_name,
            lpfnWndProc: Some(window_proc),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: bg_brush,
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
        let ctx = Arc::new(WindowContext {
            state: state.clone(),
            client: client.clone(),
            lookup_dialog_open: AtomicBool::new(false),
        });
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

        // Periodic timer to re-assert topmost — full-screen apps / UAC dialogs
        // can push us down even though WS_EX_TOPMOST is set.
        SetTimer(Some(hwnd), TIMER_ID_TOPMOST, TOPMOST_REFRESH_MS, None);

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
                    let (x, y) = compute_window_origin(hwnd, required);
                    // IMPORTANT: assert topmost on every resize — do NOT use
                    // SWP_NOZORDER, otherwise stale z-order from being demoted
                    // by full-screen apps persists.
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        x,
                        y,
                        required,
                        WIN_HEIGHT,
                        SWP_NOACTIVATE,
                    );
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
                    if !ctx_ptr.is_null() {
                        let ctx = &*ctx_ptr;
                        // Compare-and-swap on the open flag so concurrent
                        // hotkey presses can't spawn duplicate dialogs.
                        if ctx
                            .lookup_dialog_open
                            .compare_exchange(
                                false,
                                true,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            )
                            .is_ok()
                        {
                            // Take an owning Arc clone for the worker thread.
                            // This guarantees ctx outlives the dialog thread —
                            // no UAF on `lookup_dialog_open` after exit.
                            let ctx_clone = clone_ctx_arc(ctx_ptr);
                            let client = ctx_clone.client.clone();
                            let parent_hwnd_raw = hwnd.0 as usize;
                            std::thread::Builder::new()
                                .name("vpn-monitor-lookup".into())
                                .spawn(move || {
                                    let parent_hwnd =
                                        HWND(parent_hwnd_raw as *mut _);
                                    let mut dialog =
                                        super::lookup_dialog::LookupDialog::new(
                                            client,
                                        );
                                    dialog.show(parent_hwnd);
                                    // Reset the flag *after* dialog closes;
                                    // ctx_clone keeps the Arc alive until
                                    // the closure returns.
                                    ctx_clone
                                        .lookup_dialog_open
                                        .store(false, Ordering::SeqCst);
                                })
                                .ok();
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
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
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
