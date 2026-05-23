use windows::core::{s, w};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;
use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use std::sync::Arc;

use crate::network::geo_cache::GeoCache;
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

/// Custom message used to marshal a lookup result from the worker thread
/// back to the dialog's message loop. wparam carries an owned `*mut String`
/// allocated via `Box::into_raw`.
const WM_APP_LOOKUP_RESULT: u32 = WM_APP + 1;

pub struct LookupDialog {
    hwnd: HWND,
    input_hwnd: HWND,
    result_hwnd: HWND,
    error_hwnd: HWND,
    client: reqwest::Client,
    geo_cache: Option<Arc<GeoCache>>,
    /// 可选的初始 IP — 由历史窗口"双击重查"传入；WM_CREATE 完成后
    /// 自动写到输入框并立即触发一次查询。
    initial_ip: Option<String>,
}

impl LookupDialog {
    pub fn new(client: reqwest::Client, geo_cache: Option<Arc<GeoCache>>) -> Self {
        Self::with_initial_ip(client, geo_cache, None)
    }

    /// 带初始 IP 的构造 —— 历史窗口"双击行重查"走这条。
    pub fn with_initial_ip(
        client: reqwest::Client,
        geo_cache: Option<Arc<GeoCache>>,
        initial_ip: Option<String>,
    ) -> Self {
        Self {
            hwnd: HWND::default(),
            input_hwnd: HWND::default(),
            result_hwnd: HWND::default(),
            error_hwnd: HWND::default(),
            client,
            geo_cache,
            initial_ip,
        }
    }

    pub fn show(&mut self, parent: HWND) {
        unsafe {
            let hmodule = GetModuleHandleA(s!("vpn-monitor.exe")).unwrap_or_default();
            let hinstance: HINSTANCE = hmodule.into();
            let class_name = w!("VpnMonitorLookup");

            let app_icon = LoadIconW(Some(hinstance), windows::core::PCWSTR(1 as *const _))
                .unwrap_or_default();
            let wc = WNDCLASSW {
                hInstance: hinstance,
                lpszClassName: class_name,
                lpfnWndProc: Some(dialog_proc),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: GetSysColorBrush(COLOR_3DFACE),
                hIcon: app_icon,
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

            // 标题栏跟随主题（暗色模式下标题栏不再是 white-on-white 突兀）
            let mode = crate::config::load_config(Some(crate::config::config_path())).theme;
            super::theme::apply_dark_titlebar(hwnd, super::theme::is_active_dark(&mode));

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

            let _ = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("IP 地址:"),
                WS_VISIBLE | WS_CHILD,
                20,
                20,
                60,
                20,
                Some(hwnd),
                None,
                Some(hinst),
                None,
            );

            let input = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("EDIT"),
                w!(""),
                WS_VISIBLE | WS_CHILD | WS_BORDER | ES_AUTOHSCROLL_RAW,
                85,
                18,
                210,
                24,
                Some(hwnd),
                Some(HMENU(ID_INPUT_EDIT as *mut _)),
                Some(hinst),
                None,
            )
            .unwrap_or_default();

            let _ = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("BUTTON"),
                w!("查询"),
                WS_VISIBLE | WS_CHILD | super::md3::BS_OWNERDRAW_STYLE,
                305,
                17,
                70,
                26,
                Some(hwnd),
                Some(HMENU(ID_LOOKUP_BTN as *mut _)),
                Some(hinst),
                None,
            );

            let error = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!(""),
                WS_VISIBLE | WS_CHILD,
                85,
                46,
                290,
                20,
                Some(hwnd),
                Some(HMENU(ID_ERROR_STATIC as *mut _)),
                Some(hinst),
                None,
            )
            .unwrap_or_default();

            let result = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!(""),
                WS_VISIBLE | WS_CHILD,
                20,
                75,
                355,
                160,
                Some(hwnd),
                Some(HMENU(ID_RESULT_STATIC as *mut _)),
                Some(hinst),
                None,
            )
            .unwrap_or_default();

            let _ = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("BUTTON"),
                w!("复制结果"),
                WS_VISIBLE | WS_CHILD | super::md3::BS_OWNERDRAW_STYLE,
                100,
                270,
                90,
                30,
                Some(hwnd),
                Some(HMENU(ID_COPY_BTN as *mut _)),
                Some(hinst),
                None,
            );

            let _ = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("BUTTON"),
                w!("关闭"),
                WS_VISIBLE | WS_CHILD | super::md3::BS_OWNERDRAW_STYLE,
                210,
                270,
                90,
                30,
                Some(hwnd),
                Some(HMENU(ID_CLOSE_BTN as *mut _)),
                Some(hinst),
                None,
            );

            (*dialog_ptr).input_hwnd = input;
            (*dialog_ptr).result_hwnd = result;
            (*dialog_ptr).error_hwnd = error;
            (*dialog_ptr).hwnd = hwnd;

            // 若构造时带了初始 IP（历史窗口"双击重查"路径），把它写入
            // 输入框并自动触发一次查询按钮，让用户无需手动点。
            if let Some(ip) = (*dialog_ptr).initial_ip.clone() {
                set_wnd_text(input, &ip);
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_COMMAND,
                    WPARAM(ID_LOOKUP_BTN),
                    LPARAM(0),
                );
            }

            let _ = SetFocus(Some(input));

            LRESULT(0)
        }
        WM_DRAWITEM => {
            // 三个 owner-draw 按钮统一走 md3::draw_button
            let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
            let is_primary = dis.CtlID == ID_LOOKUP_BTN as u32;
            let mode = crate::config::load_config(Some(crate::config::config_path())).theme;
            let theme = super::theme::resolve(&mode);
            super::md3::draw_button(dis, &theme, is_primary);
            LRESULT(1)
        }
        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            match id {
                id if id == ID_LOOKUP_BTN => {
                    let dialog_ptr =
                        GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *mut LookupDialog;
                    if !dialog_ptr.is_null() {
                        let input_text = get_edit_text((*dialog_ptr).input_hwnd);
                        let trimmed = input_text.trim().to_string();
                        if trimmed.is_empty() {
                            set_wnd_text((*dialog_ptr).error_hwnd, "请输入IP地址");
                        } else {
                            set_wnd_text((*dialog_ptr).error_hwnd, "");

                            // Cache fast-path: if the IP is already in the
                            // geo cache (same /24, fresh) just show it without
                            // touching the network. Marks the result with
                            // [缓存] so the user knows it might be stale.
                            if let Some(cache) = &(*dialog_ptr).geo_cache {
                                if let Some(geo) = cache.get(&trimmed) {
                                    let mut text = format_geo_result(&trimmed, &geo);
                                    text.push_str("\n\n[来自本地缓存]");
                                    set_wnd_text((*dialog_ptr).result_hwnd, &text);
                                    return LRESULT(0);
                                }
                            }
                            set_wnd_text((*dialog_ptr).result_hwnd, "查询中...");

                            let client = (*dialog_ptr).client.clone();
                            let cache_clone = (*dialog_ptr).geo_cache.clone();
                            // Capture the dialog HWND as a usize for the worker;
                            // the worker posts a message back instead of touching
                            // child HWNDs directly. If the dialog is gone by the
                            // time the message would arrive, PostMessage fails
                            // and we drop the heap-allocated result safely.
                            let dialog_hwnd_raw = hwnd.0 as usize;

                            std::thread::Builder::new()
                                .name("vpn-monitor-lookup-worker".into())
                                .spawn(move || {
                                    let rt = tokio::runtime::Builder::new_current_thread()
                                        .enable_all()
                                        .build();
                                    if let Ok(rt) = rt {
                                        let geo = rt.block_on(async {
                                            let timeout =
                                                std::time::Duration::from_secs(5);
                                            // Manual lookup: prefer latency over
                                            // cross-checking — the user is staring
                                            // at the dialog waiting.
                                            geo_lookup::lookup_geo(
                                                &client, &trimmed, timeout, false,
                                            )
                                            .await
                                        });
                                        // Side-effect: populate the cache so
                                        // subsequent lookups of the same /24
                                        // hit the fast path.
                                        if let (geo_lookup::GeoLookupOutcome::Ok { geo: g, .. }, Some(cache)) =
                                            (&geo, cache_clone)
                                        {
                                            cache.insert(trimmed.clone(), g.clone());
                                        }
                                        let result_text = match geo {
                                            geo_lookup::GeoLookupOutcome::Ok { geo, warning } => {
                                                let mut t = format_geo_result(&trimmed, &geo);
                                                if let Some(w) = warning {
                                                    t.push_str("\n\n⚠ ");
                                                    t.push_str(&w);
                                                }
                                                t
                                            }
                                            geo_lookup::GeoLookupOutcome::RateLimited => {
                                                format!(
                                                    "IP: {}\n查询受限（API 限流）",
                                                    trimmed
                                                )
                                            }
                                            geo_lookup::GeoLookupOutcome::Failed(reason) => {
                                                format!(
                                                    "IP: {}\n查询失败 ({})",
                                                    trimmed,
                                                    reason.label()
                                                )
                                            }
                                        };
                                        post_result_to_dialog(dialog_hwnd_raw, result_text);
                                    }
                                })
                                .ok();
                        }
                    }
                }
                id if id == ID_COPY_BTN => {
                    let dialog_ptr =
                        GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *mut LookupDialog;
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
        msg_id if msg_id == WM_APP_LOOKUP_RESULT => {
            // Worker handed us ownership of a Boxed String via wparam.
            // We always reclaim it (free the memory) regardless of whether
            // the result HWND is still valid.
            let raw = wparam.0 as *mut String;
            if !raw.is_null() {
                let result_text = Box::from_raw(raw);
                let dialog_ptr =
                    GetWindowLongPtrA(hwnd, GWLP_USERDATA) as *mut LookupDialog;
                if !dialog_ptr.is_null() {
                    let result_hwnd = (*dialog_ptr).result_hwnd;
                    if IsWindow(Some(result_hwnd)).as_bool() {
                        set_wnd_text(result_hwnd, &result_text);
                        let _ = InvalidateRect(Some(result_hwnd), None, true);
                    }
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Worker-thread helper. Box the result, hand the pointer to the dialog's
/// message queue. If the post fails (window already destroyed), reclaim and
/// drop the Box here so we don't leak.
fn post_result_to_dialog(dialog_hwnd_raw: usize, text: String) {
    unsafe {
        let hwnd = HWND(dialog_hwnd_raw as *mut _);
        if !IsWindow(Some(hwnd)).as_bool() {
            return;
        }
        let boxed = Box::new(text);
        let raw = Box::into_raw(boxed);
        let posted = PostMessageW(
            Some(hwnd),
            WM_APP_LOOKUP_RESULT,
            WPARAM(raw as usize),
            LPARAM(0),
        );
        if posted.is_err() {
            // PostMessage failed — reclaim the box so it isn't leaked.
            drop(Box::from_raw(raw));
        }
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
            Err(_) => {
                let _ = CloseClipboard();
                return;
            }
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
