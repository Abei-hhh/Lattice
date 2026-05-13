use windows::core::{s, w};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::network::geo_lookup;
use crate::network::geo_lookup::GeoInfo;

#[allow(improper_ctypes)]
extern "system" {
    fn OpenClipboard(hwnd: Option<HWND>) -> BOOL;
    fn CloseClipboard() -> BOOL;
    fn EmptyClipboard() -> BOOL;
    fn SetClipboardData(uformat: u32, h: Option<HANDLE>) -> Option<HANDLE>;
}

const CF_UNICODETEXT_RAW: u32 = 13;
const ES_AUTOHSCROLL_RAW: WINDOW_STYLE = WINDOW_STYLE(0x0080);

const ID_INPUT_EDIT: usize = 101;
const ID_LOOKUP_BTN: usize = 102;
const ID_COPY_BTN: usize = 103;
const ID_CLOSE_BTN: usize = 104;
const ID_RESULT_STATIC: usize = 105;
const ID_ERROR_STATIC: usize = 106;

pub struct LookupDialog {
    hwnd: HWND,
    input_hwnd: HWND,
    result_hwnd: HWND,
    error_hwnd: HWND,
    client: reqwest::Client,
}

impl LookupDialog {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            hwnd: HWND::default(),
            input_hwnd: HWND::default(),
            result_hwnd: HWND::default(),
            error_hwnd: HWND::default(),
            client,
        }
    }

    pub fn show(&mut self, parent: HWND) {
        unsafe {
            let hmodule = GetModuleHandleA(s!("vpn-monitor.exe")).unwrap_or_default();
            let hinstance: HINSTANCE = hmodule.into();
            let class_name = w!("VpnMonitorLookup");

            let wc = WNDCLASSW {
                hInstance: hinstance,
                lpszClassName: class_name,
                lpfnWndProc: Some(dialog_proc),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: GetSysColorBrush(COLOR_3DFACE),
                ..Default::default()
            };

            RegisterClassW(&wc);

            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                w!("IP 地址查询"),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                400,
                350,
                Some(parent),
                None,
                Some(hinstance),
                Some(self as *mut Self as *mut _),
            ) {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!("Failed to create lookup dialog: {:?}", e);
                    return;
                }
            };

            self.hwnd = hwnd;
            center_window(hwnd);
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = UpdateWindow(hwnd);

            let mut msg = MSG::default();
            loop {
                if !IsWindow(Some(hwnd)).as_bool() {
                    break;
                }
                let got_msg = GetMessageW(&mut msg, None, 0, 0);
                if !got_msg.as_bool() {
                    break;
                }
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
        }
    }
}

fn format_geo_result(ip: &str, geo: &GeoInfo) -> String {
    format!(
        "IP:     {}\n国家:   {}\n地区:   {}\n城市:   {}\nISP:    {}",
        ip, geo.country, geo.region, geo.city, geo.isp
    )
}

fn center_window(hwnd: HWND) {
    unsafe {
        let mut rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rect);
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        let screen_width = GetSystemMetrics(SM_CXSCREEN);
        let screen_height = GetSystemMetrics(SM_CYSCREEN);
        let x = (screen_width - width) / 2;
        let y = (screen_height - height) / 2;
        let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
    }
}

fn set_wnd_text(hwnd: HWND, text: &str) {
    unsafe {
        let mut wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
        let _ = SetWindowTextW(hwnd, windows::core::PCWSTR(wide.as_mut_ptr()));
    }
}

unsafe extern "system" fn dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = lparam.0 as *const CREATESTRUCTW;
            let dialog_ptr = (*cs).lpCreateParams as *mut LookupDialog;
            SetWindowLongPtrA(hwnd, GWLP_USERDATA, dialog_ptr as isize);
            let hinst: HINSTANCE = (*cs).hInstance;

            // Label
            let _ = CreateWindowExW(
                WINDOW_EX_STYLE::default(), w!("STATIC"),
                w!("IP 地址:"),
                WS_VISIBLE | WS_CHILD, 20, 20, 60, 20,
                Some(hwnd), None, Some(hinst), None,
            );

            // Input edit
            let input = CreateWindowExW(
                WINDOW_EX_STYLE::default(), w!("EDIT"), w!(""),
                WS_VISIBLE | WS_CHILD | WS_BORDER | ES_AUTOHSCROLL_RAW,
                85, 18, 210, 24, Some(hwnd),
                Some(HMENU(ID_INPUT_EDIT as *mut _)), Some(hinst), None,
            ).unwrap_or_default();

            // Lookup button
            let _ = CreateWindowExW(
                WINDOW_EX_STYLE::default(), w!("BUTTON"),
                w!("查询"),
                WS_VISIBLE | WS_CHILD, 305, 17, 70, 26, Some(hwnd),
                Some(HMENU(ID_LOOKUP_BTN as *mut _)), Some(hinst), None,
            );

            // Error text
            let error = CreateWindowExW(
                WINDOW_EX_STYLE::default(), w!("STATIC"), w!(""),
                WS_VISIBLE | WS_CHILD, 85, 46, 290, 20, Some(hwnd),
                Some(HMENU(ID_ERROR_STATIC as *mut _)), Some(hinst), None,
            ).unwrap_or_default();

            // Result box
            let result = CreateWindowExW(
                WINDOW_EX_STYLE::default(), w!("STATIC"), w!(""),
                WS_VISIBLE | WS_CHILD, 20, 75, 355, 160, Some(hwnd),
                Some(HMENU(ID_RESULT_STATIC as *mut _)), Some(hinst), None,
            ).unwrap_or_default();

            // Copy button
            let _ = CreateWindowExW(
                WINDOW_EX_STYLE::default(), w!("BUTTON"),
                w!("复制结果"),
                WS_VISIBLE | WS_CHILD, 100, 270, 90, 30, Some(hwnd),
                Some(HMENU(ID_COPY_BTN as *mut _)), Some(hinst), None,
            );

            // Close button
            let _ = CreateWindowExW(
                WINDOW_EX_STYLE::default(), w!("BUTTON"),
                w!("关闭"),
                WS_VISIBLE | WS_CHILD, 210, 270, 90, 30, Some(hwnd),
                Some(HMENU(ID_CLOSE_BTN as *mut _)), Some(hinst), None,
            );

            (*dialog_ptr).input_hwnd = input;
            (*dialog_ptr).result_hwnd = result;
            (*dialog_ptr).error_hwnd = error;
            (*dialog_ptr).hwnd = hwnd;

            let _ = SetFocus(Some(input));

            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            match id {
                id if id == ID_LOOKUP_BTN => {
                    let dialog_ptr = GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *mut LookupDialog;
                    if !dialog_ptr.is_null() {
                        let input_text = get_edit_text((*dialog_ptr).input_hwnd);
                        let trimmed = input_text.trim().to_string();
                        if trimmed.is_empty() {
                            set_wnd_text((*dialog_ptr).error_hwnd, "请输入IP地址");
                        } else {
                            set_wnd_text((*dialog_ptr).error_hwnd, "");
                            set_wnd_text((*dialog_ptr).result_hwnd, "查询中...");

                            let client = (*dialog_ptr).client.clone();
                            let result_hwnd_raw = (*dialog_ptr).result_hwnd.0 as usize;

                            std::thread::spawn(move || {
                                let result_hwnd = HWND(result_hwnd_raw as *mut _);
                                let rt = tokio::runtime::Builder::new_current_thread()
                                    .enable_all().build();
                                if let Ok(rt) = rt {
                                    let geo = rt.block_on(async {
                                        let timeout = std::time::Duration::from_secs(5);
                                        geo_lookup::lookup_geo(&client, &trimmed, timeout).await
                                    });
                                    let result_text = match geo {
                                        geo_lookup::GeoLookupOutcome::Ok(geo) => {
                                            format_geo_result(&trimmed, &geo)
                                        }
                                        geo_lookup::GeoLookupOutcome::RateLimited => {
                                            format!(
                                                "IP: {}\n查询受限（API 限流）",
                                                trimmed
                                            )
                                        }
                                        geo_lookup::GeoLookupOutcome::Failed => {
                                            format!("IP: {}\n查询失败", trimmed)
                                        }
                                    };
                                    set_wnd_text(result_hwnd, &result_text);
                                    unsafe {
                                        let _ = InvalidateRect(Some(result_hwnd), None, true);
                                    }
                                }
                            });
                        }
                    }
                }
                id if id == ID_COPY_BTN => {
                    let dialog_ptr = GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *mut LookupDialog;
                    if !dialog_ptr.is_null() {
                        let result = get_edit_text((*dialog_ptr).result_hwnd);
                        if !result.is_empty() {
                            copy_to_clipboard(hwnd, &result);
                        }
                    }
                }
                id if id == ID_CLOSE_BTN => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn get_edit_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd) as usize;
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len + 1];
        GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..len])
    }
}

fn copy_to_clipboard(hwnd: HWND, text: &str) {
    unsafe {
        if OpenClipboard(Some(hwnd)) == BOOL(0) {
            return;
        }
        let _ = EmptyClipboard();
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
        let size = wide.len() * std::mem::size_of::<u16>();
        let h = match GlobalAlloc(GMEM_MOVEABLE, size) {
            Ok(h) => h,
            Err(_) => { let _ = CloseClipboard(); return; }
        };
        let ptr = GlobalLock(h);
        if !ptr.is_null() {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr as *mut u16, wide.len());
            let _ = GlobalUnlock(h);
            let _ = SetClipboardData(CF_UNICODETEXT_RAW, Some(HANDLE(h.0)));
        }
        let _ = CloseClipboard();
    }
}
