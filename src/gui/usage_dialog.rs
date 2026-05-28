//! 用量明细窗口 —— 浏览 cc-switch 累计用量，按 provider+model 分组。
//!
//! 顶部时间范围 radio：5h / 24h / 7d / 30d
//! 下方 ListView 列：工具 / Provider / 模型 / 请求数 / 输入Tok / 输出Tok / 费用 / 平均延迟
//! 切换时间范围 → 重新查 cc-switch SQLite 并刷新表
//! 双击行 / 右键暂未支持（信息已经在表里，复用历史窗口的复制范式即可未来扩展）

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use lattice_core::usage::{format_cost, format_tokens, list_usage_breakdown, UsageRow};

const ID_RADIO_5H: usize = 401;
const ID_RADIO_24H: usize = 402;
const ID_RADIO_7D: usize = 403;
const ID_RADIO_30D: usize = 404;
const ID_LISTVIEW: usize = 410;
const ID_CLOSE_BTN: usize = 411;

const BS_AUTORADIOBUTTON_RAW: WINDOW_STYLE = WINDOW_STYLE(0x0009);
const WS_GROUP_RAW: WINDOW_STYLE = WINDOW_STYLE(0x00020000);

const COL_APP: i32 = 0;
const COL_PROVIDER: i32 = 1;
const COL_MODEL: i32 = 2;
const COL_REQ: i32 = 3;
const COL_IN_TOK: i32 = 4;
const COL_OUT_TOK: i32 = 5;
const COL_COST: i32 = 6;
const COL_LATENCY: i32 = 7;

pub struct UsageDialog {
    hwnd: HWND,
    list_hwnd: HWND,
    current_range_secs: u64,
    rows: Vec<UsageRow>,
}

impl UsageDialog {
    pub fn new() -> Self {
        Self {
            hwnd: HWND::default(),
            list_hwnd: HWND::default(),
            current_range_secs: 5 * 3600,
            rows: Vec::new(),
        }
    }

    pub fn show(&mut self, parent: HWND) {
        unsafe {
            let hmodule = GetModuleHandleA(windows::core::s!("lattice.exe"))
                .unwrap_or_default();
            let hinstance: HINSTANCE = hmodule.into();
            let class_name = w!("LatticeUsage");

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

            let icc = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_LISTVIEW_CLASSES,
            };
            let _ = InitCommonControlsEx(&icc);

            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                w!("AI 用量明细 (cc-switch)"),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE | WS_SIZEBOX
                    | WS_MAXIMIZEBOX | WS_MINIMIZEBOX,
                CW_USEDEFAULT, CW_USEDEFAULT, 900, 504,
                Some(parent), None, Some(hinstance),
                Some(self as *mut Self as *mut _),
            ) {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!("Failed to create usage dialog: {:?}", e);
                    return;
                }
            };

            let mode = crate::config::load_config(Some(crate::config::config_path())).theme;
            super::theme::apply_dark_titlebar(hwnd, super::theme::is_active_dark(&mode));

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, Some(hwnd), 0, 0).into() {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
        }
    }
}

unsafe extern "system" fn dialog_proc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = lparam.0 as *const CREATESTRUCTW;
            if !cs.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_CREATE => create_controls(hwnd),
        WM_SIZE => { layout(hwnd); LRESULT(0) }
        WM_COMMAND => handle_command(hwnd, wparam, lparam),
        WM_DRAWITEM => {
            let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
            let is_primary = false;
            let mode = crate::config::load_config(Some(crate::config::config_path())).theme;
            let theme = super::theme::resolve(&mode);
            super::md3::draw_button(dis, &theme, is_primary);
            LRESULT(1)
        }
        WM_CLOSE => { let _ = DestroyWindow(hwnd); LRESULT(0) }
        WM_DESTROY => { PostQuitMessage(0); LRESULT(0) }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn dialog_ptr(hwnd: HWND) -> *mut UsageDialog {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut UsageDialog
}

unsafe fn create_controls(hwnd: HWND) -> LRESULT {
    let dlg_ptr = dialog_ptr(hwnd);
    if dlg_ptr.is_null() { return LRESULT(0); }
    let hinst: HINSTANCE = GetModuleHandleA(windows::core::s!("lattice.exe"))
        .unwrap_or_default().into();

    // 顶部警示行 —— 提醒用户表内数字为本地估算，可能与 Anthropic console
    // 不一致。整段 cc-switch 本地算量的方向性问题见 CLAUDE.md 搁置段。
    add_warning(hwnd, hinst,
        "⚠ 以下数字为本地估算（基于 cc-switch SQLite + jsonl 解析），可能与 Anthropic console 不一致",
        10, 10, 860);

    // 时间范围 radio 行（整体下移 24px 给警示行留位置）
    add_label(hwnd, hinst, "时间范围:", 10, 38);
    add_radio(hwnd, hinst, ID_RADIO_5H, "最近 5 小时", 80, 36, true);
    add_radio(hwnd, hinst, ID_RADIO_24H, "最近 24 小时", 200, 36, false);
    add_radio(hwnd, hinst, ID_RADIO_7D, "最近 7 天", 320, 36, false);
    add_radio(hwnd, hinst, ID_RADIO_30D, "最近 30 天", 420, 36, false);

    if let Ok(c) = GetDlgItem(Some(hwnd), ID_RADIO_5H as i32) {
        SendMessageW(c, BM_SETCHECK, Some(WPARAM(BST_CHECKED.0 as usize)), Some(LPARAM(0)));
    }

    let list = CreateWindowExW(
        WS_EX_CLIENTEDGE, WC_LISTVIEWW, w!(""),
        WS_VISIBLE | WS_CHILD | WS_TABSTOP
            | WINDOW_STYLE(LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS),
        10, 69, 860, 360,
        Some(hwnd), Some(HMENU(ID_LISTVIEW as *mut _)),
        Some(hinst), None,
    ).unwrap_or_default();

    SendMessageW(
        list, LVM_SETEXTENDEDLISTVIEWSTYLE,
        Some(WPARAM(0)),
        Some(LPARAM((LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES | LVS_EX_DOUBLEBUFFER) as isize)),
    );

    let mode = crate::config::load_config(Some(crate::config::config_path())).theme;
    if super::theme::is_active_dark(&mode) {
        let _ = SetWindowTheme(list, w!("DarkMode_Explorer"), None);
    }

    let _ = CreateWindowExW(
        WINDOW_EX_STYLE::default(), w!("BUTTON"), w!("关闭"),
        WS_VISIBLE | WS_CHILD | WS_TABSTOP | super::md3::BS_OWNERDRAW_STYLE,
        780, 439, 90, 30,
        Some(hwnd), Some(HMENU(ID_CLOSE_BTN as *mut _)),
        Some(hinst), None,
    );

    (*dlg_ptr).hwnd = hwnd;
    (*dlg_ptr).list_hwnd = list;

    setup_columns(list);
    refresh_list(&mut *dlg_ptr);

    let _ = SetFocus(Some(list));
    LRESULT(0)
}

unsafe fn add_label(parent: HWND, hinst: HINSTANCE, text: &str, x: i32, y: i32) {
    let w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        windows::core::w!("STATIC"),
        PCWSTR(w.as_ptr()),
        WS_VISIBLE | WS_CHILD,
        x, y, 70, 20,
        Some(parent), None, Some(hinst), None,
    );
}

unsafe fn add_warning(parent: HWND, hinst: HINSTANCE, text: &str, x: i32, y: i32, width: i32) {
    let w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        windows::core::w!("STATIC"),
        PCWSTR(w.as_ptr()),
        WS_VISIBLE | WS_CHILD,
        x, y, width, 20,
        Some(parent), None, Some(hinst), None,
    );
}

unsafe fn add_radio(parent: HWND, hinst: HINSTANCE, id: usize, label: &str, x: i32, y: i32, first: bool) {
    let mut style = WS_VISIBLE | WS_CHILD | WS_TABSTOP | BS_AUTORADIOBUTTON_RAW;
    if first { style |= WS_GROUP_RAW; }
    let s: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        windows::core::w!("BUTTON"),
        PCWSTR(s.as_ptr()),
        style,
        x, y, 115, 22,
        Some(parent), Some(HMENU(id as *mut _)),
        Some(hinst), None,
    );
}

unsafe fn setup_columns(list: HWND) {
    add_column(list, COL_APP, "工具", 70);
    add_column(list, COL_PROVIDER, "Provider", 140);
    add_column(list, COL_MODEL, "模型", 160);
    add_column(list, COL_REQ, "请求数", 80);
    add_column(list, COL_IN_TOK, "输入Tok", 100);
    add_column(list, COL_OUT_TOK, "输出Tok", 100);
    add_column(list, COL_COST, "费用", 90);
    add_column(list, COL_LATENCY, "平均延迟", 90);
}

unsafe fn add_column(list: HWND, idx: i32, text: &str, width: i32) {
    let mut wtext: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut col = LVCOLUMNW {
        mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM | LVCF_FMT,
        fmt: LVCFMT_LEFT, cx: width,
        pszText: PWSTR(wtext.as_mut_ptr()), iSubItem: idx,
        ..Default::default()
    };
    SendMessageW(list, LVM_INSERTCOLUMNW, Some(WPARAM(idx as usize)),
        Some(LPARAM(&mut col as *mut _ as isize)));
}

unsafe fn refresh_list(dlg: &mut UsageDialog) {
    dlg.rows = list_usage_breakdown(dlg.current_range_secs);
    SendMessageW(dlg.list_hwnd, LVM_DELETEALLITEMS, Some(WPARAM(0)), Some(LPARAM(0)));
    for (row, r) in dlg.rows.iter().enumerate() {
        let mut wapp: Vec<u16> = r.app_type.encode_utf16().chain(std::iter::once(0)).collect();
        let mut item = LVITEMW {
            mask: LVIF_TEXT, iItem: row as i32, iSubItem: 0,
            pszText: PWSTR(wapp.as_mut_ptr()),
            ..Default::default()
        };
        SendMessageW(dlg.list_hwnd, LVM_INSERTITEMW, Some(WPARAM(0)),
            Some(LPARAM(&mut item as *mut _ as isize)));
        set_subitem(dlg.list_hwnd, row as i32, COL_PROVIDER, &r.provider_id);
        set_subitem(dlg.list_hwnd, row as i32, COL_MODEL, &r.model);
        set_subitem(dlg.list_hwnd, row as i32, COL_REQ, &r.request_count.to_string());
        set_subitem(dlg.list_hwnd, row as i32, COL_IN_TOK, &format_tokens(r.input_tokens));
        set_subitem(dlg.list_hwnd, row as i32, COL_OUT_TOK, &format_tokens(r.output_tokens));
        set_subitem(dlg.list_hwnd, row as i32, COL_COST, &format_cost(r.total_cost_usd));
        set_subitem(dlg.list_hwnd, row as i32, COL_LATENCY, &format!("{}ms", r.avg_latency_ms));
    }
}

unsafe fn set_subitem(list: HWND, row: i32, col: i32, text: &str) {
    let mut wtext: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut item = LVITEMW {
        mask: LVIF_TEXT, iItem: row, iSubItem: col,
        pszText: PWSTR(wtext.as_mut_ptr()),
        ..Default::default()
    };
    SendMessageW(list, LVM_SETITEMW, Some(WPARAM(0)),
        Some(LPARAM(&mut item as *mut _ as isize)));
}

unsafe fn handle_command(hwnd: HWND, wparam: WPARAM, _lparam: LPARAM) -> LRESULT {
    let id = (wparam.0 as u32) & 0xFFFF;
    let dlg_ptr = dialog_ptr(hwnd);
    if dlg_ptr.is_null() { return LRESULT(0); }

    let new_secs = match id {
        x if x == ID_RADIO_5H as u32 => Some(5 * 3600u64),
        x if x == ID_RADIO_24H as u32 => Some(24 * 3600u64),
        x if x == ID_RADIO_7D as u32 => Some(7 * 24 * 3600u64),
        x if x == ID_RADIO_30D as u32 => Some(30 * 24 * 3600u64),
        x if x == ID_CLOSE_BTN as u32 => {
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        }
        _ => None,
    };
    if let Some(s) = new_secs {
        (*dlg_ptr).current_range_secs = s;
        refresh_list(&mut *dlg_ptr);
    }
    LRESULT(0)
}

unsafe fn layout(hwnd: HWND) {
    let dlg_ptr = dialog_ptr(hwnd);
    if dlg_ptr.is_null() { return; }
    let mut rect = RECT::default();
    let _ = GetClientRect(hwnd, &mut rect);
    let w = rect.right;
    let h = rect.bottom;

    let _ = SetWindowPos((*dlg_ptr).list_hwnd, None,
        10, 69, w - 20, h - 124, SWP_NOZORDER | SWP_NOACTIVATE);
    if let Ok(close) = GetDlgItem(Some(hwnd), ID_CLOSE_BTN as i32) {
        let _ = SetWindowPos(close, None,
            w - 100, h - 40, 90, 30, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}
