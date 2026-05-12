use tokio::sync::mpsc;
use windows::core::s;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUNDSMALL,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::AppConfig;

use super::hotkey;
use super::render::{self, IpUpdate, SharedState};

struct WindowContext {
    state: SharedState,
    shutdown_tx: mpsc::UnboundedSender<()>,
    client: reqwest::Client,
    lookup_dialog_open: bool,
    win_width: i32,
    win_height: i32,
}

struct SendHwnd(usize);
unsafe impl Send for SendHwnd {}

const WIN_WIDTH: i32 = 500;
const WIN_HEIGHT: i32 = 38;
const WIN_Y_OFFSET: i32 = 8; // distance from top of screen

pub fn create_and_run(
    config: &AppConfig,
    state: SharedState,
    mut rx: mpsc::UnboundedReceiver<IpUpdate>,
    client: reqwest::Client,
) {
    unsafe {
        let hmodule = GetModuleHandleA(None).unwrap_or_default();
        let hinstance: HINSTANCE = hmodule.into();
        let class_name = s!("VpnMonitorOverlay");

        let wc = WNDCLASSA {
            hInstance: hinstance,
            lpszClassName: class_name,
            lpfnWndProc: Some(window_proc),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            ..Default::default()
        };

        RegisterClassA(&wc);

        let screen_width = GetSystemMetrics(SM_CXSCREEN);
        let x = (screen_width - WIN_WIDTH) / 2;

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
            win_width: WIN_WIDTH,
            win_height: WIN_HEIGHT,
        });

        let hwnd = match CreateWindowExA(
            ex_style,
            class_name,
            s!(""),
            WS_POPUP | WS_VISIBLE,
            x,
            WIN_Y_OFFSET,
            WIN_WIDTH,
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

        // Smooth Win11 rounded corners via DWM (no jagged region clip).
        let pref = DWMWCP_ROUNDSMALL;
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
            };
        }
        let _ = InvalidateRect(Some(hwnd), None, true);

        // Main message loop
        loop {
            match shutdown_rx.try_recv() {
                Ok(()) => break,
                Err(_) => {}
            }

            loop {
                match rx.try_recv() {
                    Ok(update) => {
                        let mut s = state.lock().unwrap();
                        s.current_update = update;
                        drop(s);
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                }
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
                let state = (*ctx_ptr).state.lock().unwrap();
                render::paint_overlay(hwnd, &state, (*ctx_ptr).win_width, (*ctx_ptr).win_height);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // Prevent flicker - we handle all drawing in WM_PAINT
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
