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

struct WindowContext {
    state: SharedState,
    shutdown_tx: mpsc::UnboundedSender<()>,
    client: reqwest::Client,
    lookup_dialog_open: bool,
}

struct SendHwnd(usize);
unsafe impl Send for SendHwnd {}

const WIN_Y_OFFSET: i32 = 8;

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

        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<()>();

        let ctx = Box::new(WindowContext {
            state: state.clone(),
            shutdown_tx,
            client: client.clone(),
            lookup_dialog_open: false,
        });

        // Start with minimal width, will auto-size after first update
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
            Some(Box::into_raw(ctx) as *mut _),
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("Failed to create overlay window: {:?}", e);
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
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = UpdateWindow(hwnd);

        // Initial state
        {
            let mut s = state.lock().unwrap();
            s.current_update = IpUpdate {
                ip: None,
                geo: None,
                status: render::CheckStatus::Checking,
                latency_ms: None,
            };
        }

        let mut current_width: i32 = 1;
        let screen_width = GetSystemMetrics(SM_CXSCREEN);

        // Main message loop
        loop {
            match shutdown_rx.try_recv() {
                Ok(()) => break,
                Err(_) => {}
            }

            let mut need_repaint = false;

            loop {
                match rx.try_recv() {
                    Ok(UiUpdate::Ip(update)) => {
                        tracing::info!(
                            "[UI] 收到IP更新: ip={:?}, geo={:?}, status={:?}",
                            update.ip, update.geo, update.status
                        );
                        state.lock().unwrap().current_update = update;
                        need_repaint = true;
                    }
                    Ok(UiUpdate::Monitor(sample)) => {
                        let mut s = state.lock().unwrap();
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
                // Auto-size window to fit content
                let s = state.lock().unwrap();
                let hdc = GetDC(Some(hwnd));
                let required = render::measure_required_width(hdc, &s);
                ReleaseDC(Some(hwnd), hdc);
                drop(s);

                if required != current_width {
                    current_width = required;
                    let x = (screen_width - required) / 2;
                    let _ = SetWindowPos(hwnd, None, x, WIN_Y_OFFSET, required, WIN_HEIGHT, SWP_NOZORDER);
                }

                let _ = InvalidateRect(Some(hwnd), None, true);
            }

            let mut msg = MSG::default();
            while PeekMessageA(&mut msg, None, 0, 0, PM_REMOVE).into() {
                if msg.message == WM_QUIT {
                    hotkey::unregister_hotkeys(hwnd);
                    return;
                }
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageA(&msg);
            }

            std::thread::sleep(std::time::Duration::from_millis(16));
        }

        hotkey::unregister_hotkeys(hwnd);
        let _ = DestroyWindow(hwnd);
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
                let create_params = (*cs).lpCreateParams as isize;
                SetWindowLongPtrA(hwnd, GWLP_USERDATA, create_params);
            }
            DefWindowProcA(hwnd, msg, wparam, lparam)
        }
        WM_PAINT => {
            let ctx_ptr = GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *mut WindowContext;
            if !ctx_ptr.is_null() {
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                let state = (*ctx_ptr).state.lock().unwrap();
                render::paint_overlay(hwnd, &state, rect.right, rect.bottom);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            LRESULT(1)
        }
        WM_HOTKEY => {
            let hotkey_id = wparam.0 as i32;
            let ctx_ptr = GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *mut WindowContext;

            match hotkey_id {
                hotkey::HOTKEY_TOGGLE => {
                    if !ctx_ptr.is_null() {
                        let mut state = (*ctx_ptr).state.lock().unwrap();
                        state.visible = !state.visible;
                        if state.visible {
                            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        } else {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                        }
                    }
                }
                hotkey::HOTKEY_LOOKUP => {
                    if !ctx_ptr.is_null() && !(*ctx_ptr).lookup_dialog_open {
                        let client = (*ctx_ptr).client.clone();
                        (*ctx_ptr).lookup_dialog_open = true;
                        let send_hwnd = SendHwnd(hwnd.0 as usize);
                        let ctx_ptr_as_usize = ctx_ptr as usize;
                        std::thread::spawn(move || {
                            let hwnd = HWND(send_hwnd.0 as *mut _);
                            let mut dialog = super::lookup_dialog::LookupDialog::new(client);
                            dialog.show(hwnd);
                            let ctx_ptr = ctx_ptr_as_usize as *mut WindowContext;
                            if !ctx_ptr.is_null() {
                                (*ctx_ptr).lookup_dialog_open = false;
                            }
                        });
                    }
                }
                hotkey::HOTKEY_QUIT => {
                    if !ctx_ptr.is_null() {
                        let _ = (*ctx_ptr).shutdown_tx.send(());
                    }
                    PostQuitMessage(0);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcA(hwnd, msg, wparam, lparam),
    }
}
