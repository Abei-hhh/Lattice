//! 历史时间线窗口 —— 浏览 GeoCache 收集的所有 IP→Geo 记录。
//!
//! 功能：
//!   • ListView 按时间倒序展示：时间 / IP / 国家 / 城市 / ISP / 网段
//!   • 顶部搜索框：实时按子串过滤（IP/国家/城市/ISP 任一命中即保留）
//!   • 双击行：以该 IP 打开 lookup 对话框重查（一起验证缓存是否过期）
//!   • 右键行：复制 IP / 复制全部信息 / 从缓存删除
//!   • 导出 CSV：GetSaveFileNameW + UTF-8 BOM
//!   • 刷新按钮：重新从 GeoCache::history() 拉取
//!
//! 线程模型：和 lookup_dialog 一致 —— 独立 OS 线程跑自己的消息泵，
//! 不阻塞主浮窗的消息循环。

use std::sync::Arc;

use chrono::{Local, TimeZone};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::UI::Controls::Dialogs::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::network::geo_cache::{GeoCache, HistoryEntry};

// ── 控件 ID ──────────────────────────────────────────────────────
const ID_SEARCH_EDIT: usize = 201;
const ID_REFRESH_BTN: usize = 202;
const ID_LISTVIEW: usize = 203;
const ID_EXPORT_BTN: usize = 204;
const ID_CLOSE_BTN: usize = 205;

// ── 右键菜单 ID（避开托盘菜单的 8000 段） ──────────────────────
const IDM_COPY_IP: u32 = 9001;
const IDM_COPY_ALL: u32 = 9002;
const IDM_DELETE: u32 = 9003;

// ── 列索引（要和 setup_columns 一致） ───────────────────────────
const COL_TIME: i32 = 0;
const COL_IP: i32 = 1;
const COL_COUNTRY: i32 = 2;
const COL_CITY: i32 = 3;
const COL_ISP: i32 = 4;
const COL_NETKEY: i32 = 5;

// 剪贴板格式（CF_UNICODETEXT = 13）
const CF_UNICODETEXT_RAW: u32 = 13;
const ES_AUTOHSCROLL_RAW: WINDOW_STYLE = WINDOW_STYLE(0x0080);

// 剪贴板 API（windows-rs 0.59 这几个分散在不同 mod，extern 反而简洁）
#[allow(improper_ctypes)]
extern "system" {
    fn OpenClipboard(hwnd: Option<HWND>) -> BOOL;
    fn CloseClipboard() -> BOOL;
    fn EmptyClipboard() -> BOOL;
    fn SetClipboardData(uformat: u32, h: Option<HANDLE>) -> Option<HANDLE>;
}

pub struct HistoryDialog {
    hwnd: HWND,
    search_hwnd: HWND,
    list_hwnd: HWND,
    /// 完整数据快照 —— 搜索过滤时只重画列表，原始数据保留在这里
    /// 供下一次过滤 / 刷新 / 导出使用。
    all_entries: Vec<HistoryEntry>,
    /// 当前显示的（已过滤）条目。每行的网段 key 用于"删除"右键操作。
    visible_entries: Vec<HistoryEntry>,
    geo_cache: Option<Arc<GeoCache>>,
}

impl HistoryDialog {
    pub fn new(geo_cache: Option<Arc<GeoCache>>) -> Self {
        let all = geo_cache
            .as_ref()
            .map(|c| c.history())
            .unwrap_or_default();
        Self {
            hwnd: HWND::default(),
            search_hwnd: HWND::default(),
            list_hwnd: HWND::default(),
            visible_entries: all.clone(),
            all_entries: all,
            geo_cache,
        }
    }

    pub fn show(&mut self, parent: HWND) {
        unsafe {
            // 注册一次窗口类（多次注册无害，OS 会返回 ERROR_CLASS_ALREADY_EXISTS）
            let hmodule = GetModuleHandleA(windows::core::s!("lattice.exe"))
                .unwrap_or_default();
            let hinstance: HINSTANCE = hmodule.into();
            let class_name = w!("LatticeHistory");

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

            // SysListView32 在使用前必须 InitCommonControlsEx 注册类
            let icc = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_LISTVIEW_CLASSES,
            };
            let _ = InitCommonControlsEx(&icc);

            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                w!("IP 历史记录"),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE | WS_SIZEBOX | WS_MAXIMIZEBOX | WS_MINIMIZEBOX,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                820,
                540,
                Some(parent),
                None,
                Some(hinstance),
                Some(self as *mut Self as *mut _),
            ) {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!("Failed to create history dialog: {:?}", e);
                    return;
                }
            };

            let mode = crate::config::load_config(Some(crate::config::config_path())).theme;
            let dark = super::theme::is_active_dark(&mode);
            super::theme::apply_dark_titlebar(hwnd, dark);

            // 跑自己的消息泵 —— 用 GetMessage 阻塞等待，因为这个对话框不需要
            // 像主浮窗那样定时唤醒检查 mpsc 通道。
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, Some(hwnd), 0, 0).into() {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
        }
    }
}

unsafe extern "system" fn dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            // 把 HistoryDialog 指针塞到窗口的 user data，后续消息能 O(1) 取回
            let cs = lparam.0 as *const CREATESTRUCTW;
            if !cs.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_CREATE => create_controls(hwnd),
        WM_SIZE => {
            layout(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => handle_command(hwnd, wparam, lparam),
        WM_DRAWITEM => {
            let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
            let id = dis.CtlID;
            let is_primary = id == ID_REFRESH_BTN as u32;
            let mode = crate::config::load_config(Some(crate::config::config_path())).theme;
            let theme = super::theme::resolve(&mode);
            super::md3::draw_button(dis, &theme, is_primary);
            LRESULT(1)
        }
        WM_NOTIFY => handle_notify(hwnd, lparam),
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn dialog_ptr(hwnd: HWND) -> *mut HistoryDialog {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut HistoryDialog
}

// ── 控件创建 ────────────────────────────────────────────────────

unsafe fn create_controls(hwnd: HWND) -> LRESULT {
    let dlg_ptr = dialog_ptr(hwnd);
    if dlg_ptr.is_null() {
        return LRESULT(0);
    }
    let hinst: HINSTANCE = GetModuleHandleA(windows::core::s!("lattice.exe"))
        .unwrap_or_default()
        .into();

    // 顶部"搜索:"静态文本
    let _ = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("STATIC"),
        w!("搜索:"),
        WS_VISIBLE | WS_CHILD,
        10, 14, 50, 20,
        Some(hwnd), None, Some(hinst), None,
    );

    let search = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        w!("EDIT"),
        w!(""),
        WS_VISIBLE | WS_CHILD | WS_TABSTOP | ES_AUTOHSCROLL_RAW,
        65, 10, 500, 24,
        Some(hwnd),
        Some(HMENU(ID_SEARCH_EDIT as *mut _)),
        Some(hinst),
        None,
    ).unwrap_or_default();

    let _ = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("BUTTON"),
        w!("刷新"),
        WS_VISIBLE | WS_CHILD | WS_TABSTOP | super::md3::BS_OWNERDRAW_STYLE,
        575, 10, 70, 26,
        Some(hwnd),
        Some(HMENU(ID_REFRESH_BTN as *mut _)),
        Some(hinst),
        None,
    );

    // ListView —— 报表视图 + 完整行选择 + 网格线
    let list = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        WC_LISTVIEWW,
        w!(""),
        WS_VISIBLE | WS_CHILD | WS_TABSTOP | WINDOW_STYLE(LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS),
        10, 45, 780, 410,
        Some(hwnd),
        Some(HMENU(ID_LISTVIEW as *mut _)),
        Some(hinst),
        None,
    ).unwrap_or_default();

    // 扩展样式 —— 必须用 LVM_SETEXTENDEDLISTVIEWSTYLE 单独发，
    // 因为窗口创建参数里的 dwStyle 只接 LVS_* 不接 LVS_EX_*。
    SendMessageW(
        list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        Some(WPARAM(0)),
        Some(LPARAM((LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES | LVS_EX_DOUBLEBUFFER) as isize)),
    );

    // 跟随主题：暗色时让 ListView 走 explorer 的暗色样式，标题栏、选中
    // 高亮、滚动条都变暗。亮色时恢复默认主题。
    let mode = crate::config::load_config(Some(crate::config::config_path())).theme;
    if super::theme::is_active_dark(&mode) {
        let _ = windows::Win32::UI::Controls::SetWindowTheme(
            list,
            windows::core::w!("DarkMode_Explorer"),
            None,
        );
    }

    let _ = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("BUTTON"),
        w!("导出 CSV..."),
        WS_VISIBLE | WS_CHILD | WS_TABSTOP | super::md3::BS_OWNERDRAW_STYLE,
        600, 465, 90, 30,
        Some(hwnd),
        Some(HMENU(ID_EXPORT_BTN as *mut _)),
        Some(hinst),
        None,
    );

    let _ = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("BUTTON"),
        w!("关闭"),
        WS_VISIBLE | WS_CHILD | WS_TABSTOP | super::md3::BS_OWNERDRAW_STYLE,
        700, 465, 90, 30,
        Some(hwnd),
        Some(HMENU(ID_CLOSE_BTN as *mut _)),
        Some(hinst),
        None,
    );

    (*dlg_ptr).hwnd = hwnd;
    (*dlg_ptr).search_hwnd = search;
    (*dlg_ptr).list_hwnd = list;

    setup_columns(list);
    populate_list(&*dlg_ptr);

    let _ = SetFocus(Some(search));
    LRESULT(0)
}

unsafe fn setup_columns(list: HWND) {
    // 列宽 = 像素；总和 ~770 留给滚动条余地
    add_column(list, COL_TIME, "时间", 150);
    add_column(list, COL_IP, "IP", 130);
    add_column(list, COL_COUNTRY, "国家", 90);
    add_column(list, COL_CITY, "城市", 120);
    add_column(list, COL_ISP, "ISP", 180);
    add_column(list, COL_NETKEY, "网段", 90);
}

unsafe fn add_column(list: HWND, idx: i32, text: &str, width: i32) {
    let mut wtext: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut col = LVCOLUMNW {
        mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM | LVCF_FMT,
        fmt: LVCFMT_LEFT,
        cx: width,
        pszText: PWSTR(wtext.as_mut_ptr()),
        iSubItem: idx,
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_INSERTCOLUMNW,
        Some(WPARAM(idx as usize)),
        Some(LPARAM(&mut col as *mut _ as isize)),
    );
}

// ── 数据填充 ────────────────────────────────────────────────────

/// 把 `dlg.visible_entries` 写入 ListView。会先清空再批量插入，避免
/// 每次过滤都拼写差异。复杂度 O(n) 但 1000 行内秒级完成。
unsafe fn populate_list(dlg: &HistoryDialog) {
    SendMessageW(dlg.list_hwnd, LVM_DELETEALLITEMS, Some(WPARAM(0)), Some(LPARAM(0)));

    for (row, entry) in dlg.visible_entries.iter().enumerate() {
        let time_str = format_time(entry.inserted_at);
        let mut wtime: Vec<u16> =
            time_str.encode_utf16().chain(std::iter::once(0)).collect();
        let mut item = LVITEMW {
            mask: LVIF_TEXT,
            iItem: row as i32,
            iSubItem: 0,
            pszText: PWSTR(wtime.as_mut_ptr()),
            ..Default::default()
        };
        SendMessageW(
            dlg.list_hwnd,
            LVM_INSERTITEMW,
            Some(WPARAM(0)),
            Some(LPARAM(&mut item as *mut _ as isize)),
        );

        set_subitem(dlg.list_hwnd, row as i32, COL_IP, &entry.last_ip);
        set_subitem(dlg.list_hwnd, row as i32, COL_COUNTRY, &entry.geo.country);
        set_subitem(dlg.list_hwnd, row as i32, COL_CITY, &entry.geo.city);
        set_subitem(dlg.list_hwnd, row as i32, COL_ISP, &entry.geo.isp);
        set_subitem(dlg.list_hwnd, row as i32, COL_NETKEY, &entry.key);
    }
}

unsafe fn set_subitem(list: HWND, row: i32, col: i32, text: &str) {
    let mut wtext: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut item = LVITEMW {
        mask: LVIF_TEXT,
        iItem: row,
        iSubItem: col,
        pszText: PWSTR(wtext.as_mut_ptr()),
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_SETITEMW,
        Some(WPARAM(0)),
        Some(LPARAM(&mut item as *mut _ as isize)),
    );
}

fn format_time(unix_secs: u64) -> String {
    // 用本地时区渲染。Local.timestamp_opt 返回 LocalResult，take single
    // 失败的话用 "?" 兜底（应该几乎不发生）。
    match Local.timestamp_opt(unix_secs as i64, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => "?".to_string(),
    }
}

// ── 命令路由 ────────────────────────────────────────────────────

unsafe fn handle_command(hwnd: HWND, wparam: WPARAM, _lparam: LPARAM) -> LRESULT {
    let dlg_ptr = dialog_ptr(hwnd);
    if dlg_ptr.is_null() {
        return LRESULT(0);
    }
    let id = (wparam.0 as u32) & 0xFFFF;
    let notify = ((wparam.0 as u32) >> 16) & 0xFFFF;

    match id {
        x if x == ID_SEARCH_EDIT as u32 && notify == EN_CHANGE => {
            apply_filter(&mut *dlg_ptr);
        }
        x if x == ID_REFRESH_BTN as u32 => {
            // 重新从 cache 拉取（外部 IP poll 任务可能写入了新条目）
            if let Some(cache) = &(*dlg_ptr).geo_cache {
                (*dlg_ptr).all_entries = cache.history();
            }
            apply_filter(&mut *dlg_ptr);
        }
        x if x == ID_EXPORT_BTN as u32 => {
            export_csv(hwnd, &*dlg_ptr);
        }
        x if x == ID_CLOSE_BTN as u32 => {
            let _ = DestroyWindow(hwnd);
        }
        // 右键菜单分发
        x if x == IDM_COPY_IP => {
            if let Some(entry) = selected_entry(&*dlg_ptr) {
                copy_to_clipboard(hwnd, &entry.last_ip);
            }
        }
        x if x == IDM_COPY_ALL => {
            if let Some(entry) = selected_entry(&*dlg_ptr) {
                let text = format!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    format_time(entry.inserted_at),
                    entry.last_ip,
                    entry.geo.country,
                    entry.geo.city,
                    entry.geo.isp,
                    entry.key,
                );
                copy_to_clipboard(hwnd, &text);
            }
        }
        x if x == IDM_DELETE => {
            if let Some(entry) = selected_entry(&*dlg_ptr) {
                if let Some(cache) = &(*dlg_ptr).geo_cache {
                    if cache.remove(&entry.key) {
                        (*dlg_ptr).all_entries = cache.history();
                        apply_filter(&mut *dlg_ptr);
                    }
                }
            }
        }
        _ => {}
    }
    LRESULT(0)
}

// ── WM_NOTIFY（ListView 双击 / 右键） ───────────────────────────

unsafe fn handle_notify(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let nmhdr = &*(lparam.0 as *const NMHDR);
    if nmhdr.idFrom != ID_LISTVIEW {
        return LRESULT(0);
    }
    let dlg_ptr = dialog_ptr(hwnd);
    if dlg_ptr.is_null() {
        return LRESULT(0);
    }
    match nmhdr.code {
        NM_RCLICK => {
            // 右键 = 上下文菜单
            if selected_entry(&*dlg_ptr).is_some() {
                show_context_menu(hwnd);
            }
        }
        _ => {}
    }
    LRESULT(0)
}

/// 取当前选中行对应的 HistoryEntry（拷贝），无选中返回 None。
unsafe fn selected_entry(dlg: &HistoryDialog) -> Option<HistoryEntry> {
    let idx = SendMessageW(
        dlg.list_hwnd,
        LVM_GETNEXTITEM,
        Some(WPARAM(usize::MAX)), // -1 = 从开头找
        Some(LPARAM(LVNI_SELECTED as isize)),
    )
    .0;
    if idx < 0 {
        return None;
    }
    dlg.visible_entries.get(idx as usize).cloned()
}

unsafe fn show_context_menu(hwnd: HWND) {
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };
    add_menu_item(menu, IDM_COPY_IP, "复制 IP");
    add_menu_item(menu, IDM_COPY_ALL, "复制完整行");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    add_menu_item(menu, IDM_DELETE, "从缓存删除");

    let _ = SetForegroundWindow(hwnd);
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, Some(0), hwnd, None);
    let _ = DestroyMenu(menu);
}

unsafe fn add_menu_item(menu: HMENU, id: u32, label: &str) {
    let w: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = AppendMenuW(menu, MF_STRING, id as usize, PCWSTR(w.as_ptr()));
}

// ── 过滤 ────────────────────────────────────────────────────────

/// 读搜索框文本，过滤 all_entries → visible_entries → 重画 ListView。
/// 空搜索串显示全部。子串大小写不敏感。
unsafe fn apply_filter(dlg: &mut HistoryDialog) {
    let mut buf = [0u16; 256];
    let len = GetWindowTextW(dlg.search_hwnd, &mut buf);
    let query = String::from_utf16_lossy(&buf[..len as usize])
        .trim()
        .to_lowercase();

    if query.is_empty() {
        dlg.visible_entries = dlg.all_entries.clone();
    } else {
        dlg.visible_entries = dlg
            .all_entries
            .iter()
            .filter(|e| {
                e.last_ip.to_lowercase().contains(&query)
                    || e.geo.country.to_lowercase().contains(&query)
                    || e.geo.city.to_lowercase().contains(&query)
                    || e.geo.isp.to_lowercase().contains(&query)
                    || e.key.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();
    }
    populate_list(dlg);
}

// ── CSV 导出 ────────────────────────────────────────────────────

/// 弹 GetSaveFileNameW 选路径，把当前显示的行写 UTF-8 BOM + CSV。
/// 字段里有 `,` 或 `"` 会用 RFC 4180 的双引号转义。
unsafe fn export_csv(hwnd: HWND, dlg: &HistoryDialog) {
    let mut filename = [0u16; 260];
    let initial = "ip_history.csv".encode_utf16().collect::<Vec<u16>>();
    filename[..initial.len()].copy_from_slice(&initial);

    let filter: Vec<u16> = "CSV 文件 (*.csv)\0*.csv\0全部文件 (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title: Vec<u16> = "导出 IP 历史"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let default_ext: Vec<u16> = "csv".encode_utf16().chain(std::iter::once(0)).collect();

    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: hwnd,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(filename.as_mut_ptr()),
        nMaxFile: filename.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        lpstrDefExt: PCWSTR(default_ext.as_ptr()),
        Flags: OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST,
        ..Default::default()
    };

    if !GetSaveFileNameW(&mut ofn).as_bool() {
        return; // 用户取消
    }

    // 拼 CSV 内容
    let mut text = String::from("\u{FEFF}"); // UTF-8 BOM，Excel 才识别中文
    text.push_str("时间,IP,国家,城市,ISP,网段\n");
    for e in &dlg.visible_entries {
        text.push_str(&format!(
            "{},{},{},{},{},{}\n",
            csv_escape(&format_time(e.inserted_at)),
            csv_escape(&e.last_ip),
            csv_escape(&e.geo.country),
            csv_escape(&e.geo.city),
            csv_escape(&e.geo.isp),
            csv_escape(&e.key),
        ));
    }

    let path = String::from_utf16_lossy(&filename)
        .trim_end_matches('\u{0}')
        .to_string();
    if let Err(e) = std::fs::write(&path, text) {
        let msg: Vec<u16> = format!("写入失败: {}", e)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let t: Vec<u16> = "导出错误".encode_utf16().chain(std::iter::once(0)).collect();
        MessageBoxW(
            Some(hwnd),
            PCWSTR(msg.as_ptr()),
            PCWSTR(t.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// ── 剪贴板 ──────────────────────────────────────────────────────

unsafe fn copy_to_clipboard(hwnd: HWND, text: &str) {
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };

    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;
    let hmem = match GlobalAlloc(GMEM_MOVEABLE, bytes) {
        Ok(h) => h,
        Err(_) => return,
    };
    let ptr = GlobalLock(hmem) as *mut u16;
    if ptr.is_null() {
        return;
    }
    std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
    let _ = GlobalUnlock(hmem);

    if OpenClipboard(Some(hwnd)).as_bool() {
        let _ = EmptyClipboard();
        SetClipboardData(CF_UNICODETEXT_RAW, Some(HANDLE(hmem.0 as *mut _)));
        let _ = CloseClipboard();
    }
}

// ── 自适应布局 ──────────────────────────────────────────────────

/// 处理 WM_SIZE —— 让 ListView 占满中间区域，按钮贴底。
unsafe fn layout(hwnd: HWND) {
    let dlg_ptr = dialog_ptr(hwnd);
    if dlg_ptr.is_null() {
        return;
    }
    let mut rect = RECT::default();
    let _ = GetClientRect(hwnd, &mut rect);
    let w = rect.right;
    let h = rect.bottom;

    // 搜索框宽度 = 窗口宽 - 左边距(65) - 刷新按钮(80) - 右边距(10)
    let search_w = (w - 65 - 80 - 10).max(100);
    let _ = SetWindowPos(
        (*dlg_ptr).search_hwnd,
        None,
        65, 10, search_w, 24,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    // 刷新按钮
    if let Ok(refresh) = GetDlgItem(Some(hwnd), ID_REFRESH_BTN as i32) {
        let _ = SetWindowPos(refresh, None, 65 + search_w + 5, 10, 70, 26, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    // ListView
    let _ = SetWindowPos(
        (*dlg_ptr).list_hwnd,
        None,
        10, 45, w - 20, h - 100,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    // 底部按钮（右对齐）
    if let Ok(export) = GetDlgItem(Some(hwnd), ID_EXPORT_BTN as i32) {
        let _ = SetWindowPos(export, None, w - 200, h - 40, 90, 30, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    if let Ok(close) = GetDlgItem(Some(hwnd), ID_CLOSE_BTN as i32) {
        let _ = SetWindowPos(close, None, w - 100, h - 40, 90, 30, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}
