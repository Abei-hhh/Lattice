//! 高级设置对话框 —— Tab 分组的全字段编辑器。
//!
//! 五个 Tab：常规 / 网络 / 隐私&安全 / 热键 / 高级
//!
//! 保存路径：
//!   1. 从所有控件读当前值
//!   2. 用 `toml_edit` 加载 config.toml，**保留注释和顺序**，逐 key 更新
//!   3. 原子写回 config.toml (tmp + rename)
//!   4. 立即生效字段直接应用到运行态（OnceLock / AtomicBool / window 属性）
//!   5. 需重启字段在 MessageBox 里提示用户
//!
//! Tab 实现：所有控件一次性创建，按 tab 索引 ShowWindow(SW_SHOW / SW_HIDE)。
//! 切 tab 时不重建任何控件，状态保留在控件里。

use std::sync::Arc;

use toml_edit::{value, DocumentMut};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{self, AppConfig};
use crate::network::ip_fetcher;
use crate::runtime::RuntimeFlags;

const ID_TAB: usize = 300;
const ID_OK: usize = 301;
const ID_CANCEL: usize = 302;
const ID_APPLY: usize = 303;

// 各 tab 上控件 ID（统一从 1000 开始，按 tab+字段编号）
// 常规 tab (1000s)
const ID_CHECK_INTERVAL: usize = 1001;
const ID_OPACITY: usize = 1002;
const ID_CLICK_THROUGH: usize = 1003;
const ID_ENABLE_LOG: usize = 1004;
// theme radio 组（system / light / dark）— 1010..=1012
const ID_THEME_SYSTEM: usize = 1010;
const ID_THEME_LIGHT: usize = 1011;
const ID_THEME_DARK: usize = 1012;
// 网络 tab (2000s)
const ID_TIMEOUT: usize = 2001;
const ID_MAX_RETRIES: usize = 2002;
const ID_MONITOR_INTERVAL: usize = 2003;
const ID_PROXY_CHECK_INTERVAL: usize = 2004;
const ID_MODEL_REFRESH_INTERVAL: usize = 2005;
const ID_PROXY: usize = 2006;
// 隐私 tab (3000s)
const ID_MASK_IP: usize = 3001;
const ID_MASK_GEO: usize = 3002;
const ID_CROSS_CHECK: usize = 3003;
// 热键 tab (4000s)
const ID_HOTKEY_TOGGLE: usize = 4001;
const ID_HOTKEY_LOOKUP: usize = 4002;
const ID_HOTKEY_QUIT: usize = 4003;
// 高级 tab (5000s)
const ID_GEO_CACHE_ENABLED: usize = 5001;
const ID_GEO_CACHE_TTL: usize = 5002;
const ID_GEO_CACHE_MAX: usize = 5003;
const ID_IDLE_THRESHOLD: usize = 5004;
const ID_IDLE_MULTIPLIER: usize = 5005;
// cc-switch 源 radio 组：5100 + tool index（与 KNOWN_TOOLS 顺序对应）
const ID_CCSWITCH_RADIO_BASE: usize = 5100;

const TAB_COUNT: usize = 5;
const ES_NUMBER_RAW: WINDOW_STYLE = WINDOW_STYLE(0x2000);
const ES_AUTOHSCROLL_RAW: WINDOW_STYLE = WINDOW_STYLE(0x0080);
const BS_AUTOCHECKBOX_RAW: WINDOW_STYLE = WINDOW_STYLE(0x0003);
const BS_AUTORADIOBUTTON_RAW: WINDOW_STYLE = WINDOW_STYLE(0x0009);
const WS_GROUP_RAW: WINDOW_STYLE = WINDOW_STYLE(0x00020000);

// 控件分组 —— 每个数组保存属于该 tab 的所有控件 ID（label + 控件本身）。
// 用于 tab 切换时批量 ShowWindow。
const TAB_IDS: [&[usize]; TAB_COUNT] = [
    &[
        ID_CHECK_INTERVAL, ID_OPACITY, ID_CLICK_THROUGH, ID_ENABLE_LOG,
        ID_THEME_SYSTEM, ID_THEME_LIGHT, ID_THEME_DARK,
    ],
    &[
        ID_TIMEOUT,
        ID_MAX_RETRIES,
        ID_MONITOR_INTERVAL,
        ID_PROXY_CHECK_INTERVAL,
        ID_MODEL_REFRESH_INTERVAL,
        ID_PROXY,
    ],
    &[ID_MASK_IP, ID_MASK_GEO, ID_CROSS_CHECK],
    &[ID_HOTKEY_TOGGLE, ID_HOTKEY_LOOKUP, ID_HOTKEY_QUIT],
    &[
        ID_GEO_CACHE_ENABLED,
        ID_GEO_CACHE_TTL,
        ID_GEO_CACHE_MAX,
        ID_IDLE_THRESHOLD,
        ID_IDLE_MULTIPLIER,
        ID_CCSWITCH_RADIO_BASE,
        ID_CCSWITCH_RADIO_BASE + 1,
        ID_CCSWITCH_RADIO_BASE + 2,
        ID_CCSWITCH_RADIO_BASE + 3,
        ID_CCSWITCH_RADIO_BASE + 4,
        ID_CCSWITCH_RADIO_BASE + 5,
    ],
];

// label 控件 ID 是字段 ID + 10000，独立保存以便随字段一起 show/hide。
fn label_id(field_id: usize) -> usize {
    field_id + 10000
}

pub struct SettingsDialog {
    hwnd: HWND,
    tab_hwnd: HWND,
    current_tab: usize,
    runtime_flags: Arc<RuntimeFlags>,
    /// 父浮窗 HWND —— 应用 opacity 时需要直接操作主浮窗。
    overlay_hwnd: HWND,
    /// 启动时读到的配置 —— 控件初值来自这里，应用时用于 diff 判断
    /// "数值字段是否变了从而需要弹重启提示"。
    initial: AppConfig,
}

impl SettingsDialog {
    pub fn new(runtime_flags: Arc<RuntimeFlags>, overlay_hwnd: HWND) -> Self {
        // 每次打开都从磁盘 fresh load，避免显示陈旧配置
        let initial = config::load_config(Some(config::config_path()));
        Self {
            hwnd: HWND::default(),
            tab_hwnd: HWND::default(),
            current_tab: 0,
            runtime_flags,
            overlay_hwnd,
            initial,
        }
    }

    pub fn show(&mut self, parent: HWND) {
        unsafe {
            let hmodule = GetModuleHandleA(windows::core::s!("vpn-monitor.exe"))
                .unwrap_or_default();
            let hinstance: HINSTANCE = hmodule.into();
            let class_name = w!("VpnMonitorSettings");

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

            // SysTabControl32 也是 common control，记得初始化
            let icc = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_TAB_CLASSES,
            };
            let _ = InitCommonControlsEx(&icc);

            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                w!("高级设置"),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                560,
                450,
                Some(parent),
                None,
                Some(hinstance),
                Some(self as *mut Self as *mut _),
            ) {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!("Failed to create settings dialog: {:?}", e);
                    return;
                }
            };

            super::theme::apply_dark_titlebar(
                hwnd,
                super::theme::is_active_dark(&self.initial.theme),
            );

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
            let cs = lparam.0 as *const CREATESTRUCTW;
            if !cs.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_CREATE => create_all_controls(hwnd),
        WM_COMMAND => handle_command(hwnd, wparam, lparam),
        WM_DRAWITEM => {
            let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
            let id = dis.CtlID;
            // 主操作（确定）用 primary 色，其它按钮用次要色
            let is_primary = id == ID_OK as u32;
            // 取当前主题 —— 启动时已读到 dlg.initial.theme，这里临时再 resolve
            // 一次（最快路径；切主题时设置对话框还未实时跟，下次打开生效）
            let dlg_ptr = dialog_ptr(hwnd);
            let theme = if !dlg_ptr.is_null() {
                super::theme::resolve(&(*dlg_ptr).initial.theme)
            } else {
                super::theme::DARK
            };
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

unsafe fn dialog_ptr(hwnd: HWND) -> *mut SettingsDialog {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsDialog
}

// ── 控件创建 ────────────────────────────────────────────────────

unsafe fn create_all_controls(hwnd: HWND) -> LRESULT {
    let dlg_ptr = dialog_ptr(hwnd);
    if dlg_ptr.is_null() {
        return LRESULT(0);
    }
    let hinst: HINSTANCE = GetModuleHandleA(windows::core::s!("vpn-monitor.exe"))
        .unwrap_or_default()
        .into();

    // Tab 控件，占顶部
    let tab = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        WC_TABCONTROLW,
        w!(""),
        WS_VISIBLE | WS_CHILD | WS_TABSTOP,
        10, 10, 530, 340,
        Some(hwnd),
        Some(HMENU(ID_TAB as *mut _)),
        Some(hinst),
        None,
    ).unwrap_or_default();

    add_tab(tab, 0, "常规");
    add_tab(tab, 1, "网络");
    add_tab(tab, 2, "隐私 & 安全");
    add_tab(tab, 3, "热键");
    add_tab(tab, 4, "高级");

    let cfg = &(*dlg_ptr).initial;

    // ── Tab 0: 常规 ───
    add_label(hwnd, hinst, label_id(ID_CHECK_INTERVAL), "IP 检测间隔（秒）:", 30, 50);
    add_edit_num(hwnd, hinst, ID_CHECK_INTERVAL, 220, 48, &cfg.check_interval.to_string());

    add_label(hwnd, hinst, label_id(ID_OPACITY), "不透明度 (0.0 - 1.0):", 30, 90);
    add_edit(hwnd, hinst, ID_OPACITY, 220, 88, 80, &format!("{:.2}", cfg.opacity));

    add_check(hwnd, hinst, ID_CLICK_THROUGH, "鼠标穿透（不响应任何点击）", 30, 130, cfg.click_through);
    add_check(hwnd, hinst, ID_ENABLE_LOG, "启用日志（写到 %APPDATA%\\Vpn_Monitor）", 30, 160, cfg.enable_log);

    // 主题 radio 组 —— 与 cc-switch 那组用不同 WS_GROUP 分隔
    add_label(hwnd, hinst, label_id(ID_THEME_SYSTEM), "主题:", 30, 200);
    add_radio(hwnd, hinst, ID_THEME_SYSTEM, "跟随系统", 90, 198, true);
    add_radio(hwnd, hinst, ID_THEME_LIGHT, "白天", 220, 198, false);
    add_radio(hwnd, hinst, ID_THEME_DARK, "黑夜", 320, 198, false);
    let theme_id = match cfg.theme.as_str() {
        "light" => ID_THEME_LIGHT,
        "dark" => ID_THEME_DARK,
        _ => ID_THEME_SYSTEM,
    };
    if let Ok(c) = GetDlgItem(Some(hwnd), theme_id as i32) {
        SendMessageW(c, BM_SETCHECK, Some(WPARAM(BST_CHECKED.0 as usize)), Some(LPARAM(0)));
    }

    // ── Tab 1: 网络 ───
    add_label(hwnd, hinst, label_id(ID_TIMEOUT), "请求超时（秒）:", 30, 50);
    add_edit_num(hwnd, hinst, ID_TIMEOUT, 220, 48, &cfg.timeout.to_string());

    add_label(hwnd, hinst, label_id(ID_MAX_RETRIES), "失败重试次数:", 30, 80);
    add_edit_num(hwnd, hinst, ID_MAX_RETRIES, 220, 78, &cfg.max_retries.to_string());

    add_label(hwnd, hinst, label_id(ID_MONITOR_INTERVAL), "监控刷新间隔（秒）:", 30, 110);
    add_edit_num(hwnd, hinst, ID_MONITOR_INTERVAL, 220, 108, &cfg.monitor_interval.to_string());

    add_label(hwnd, hinst, label_id(ID_PROXY_CHECK_INTERVAL), "代理检测间隔（秒）:", 30, 140);
    add_edit_num(hwnd, hinst, ID_PROXY_CHECK_INTERVAL, 220, 138, &cfg.proxy_check_interval.to_string());

    add_label(hwnd, hinst, label_id(ID_MODEL_REFRESH_INTERVAL), "Claude 标签刷新（秒）:", 30, 170);
    add_edit_num(hwnd, hinst, ID_MODEL_REFRESH_INTERVAL, 220, 168, &cfg.model_refresh_interval.to_string());

    add_label(hwnd, hinst, label_id(ID_PROXY), "代理 URL (可选):", 30, 200);
    add_edit(hwnd, hinst, ID_PROXY, 30, 220, 470, cfg.proxy.as_deref().unwrap_or(""));

    // ── Tab 2: 隐私 ───
    add_check(hwnd, hinst, ID_MASK_IP, "日志中掩码公网 IP (1.2.x.x)", 30, 50, cfg.mask_ip_in_log);
    add_check(hwnd, hinst, ID_MASK_GEO, "日志中脱敏归属地（hash 替换）", 30, 80, cfg.mask_geo_in_log);
    add_check(hwnd, hinst, ID_CROSS_CHECK, "HTTPS / HTTP 跨源国别校验", 30, 110, cfg.geo_cross_check);

    // ── Tab 3: 热键 ───
    add_label(hwnd, hinst, label_id(ID_HOTKEY_TOGGLE), "显隐浮窗:", 30, 50);
    add_edit(hwnd, hinst, ID_HOTKEY_TOGGLE, 220, 48, 200, &cfg.hotkey_toggle);

    add_label(hwnd, hinst, label_id(ID_HOTKEY_LOOKUP), "打开 IP 查询:", 30, 80);
    add_edit(hwnd, hinst, ID_HOTKEY_LOOKUP, 220, 78, 200, &cfg.hotkey_lookup);

    add_label(hwnd, hinst, label_id(ID_HOTKEY_QUIT), "退出程序:", 30, 110);
    add_edit(hwnd, hinst, ID_HOTKEY_QUIT, 220, 108, 200, &cfg.hotkey_quit);

    // ── Tab 4: 高级 ───
    add_check(hwnd, hinst, ID_GEO_CACHE_ENABLED, "启用归属地磁盘缓存", 30, 50, cfg.geo_cache_enabled);

    add_label(hwnd, hinst, label_id(ID_GEO_CACHE_TTL), "缓存有效期（小时）:", 30, 80);
    add_edit_num(hwnd, hinst, ID_GEO_CACHE_TTL, 220, 78, &cfg.geo_cache_ttl_hours.to_string());

    add_label(hwnd, hinst, label_id(ID_GEO_CACHE_MAX), "缓存最大条目:", 30, 110);
    add_edit_num(hwnd, hinst, ID_GEO_CACHE_MAX, 220, 108, &cfg.geo_cache_max_entries.to_string());

    add_label(hwnd, hinst, label_id(ID_IDLE_THRESHOLD), "空闲阈值（秒）, 0 关闭:", 30, 140);
    add_edit_num(hwnd, hinst, ID_IDLE_THRESHOLD, 220, 138, &cfg.idle_threshold_seconds.to_string());

    add_label(hwnd, hinst, label_id(ID_IDLE_MULTIPLIER), "空闲时间隔倍数:", 30, 170);
    add_edit_num(hwnd, hinst, ID_IDLE_MULTIPLIER, 220, 168, &cfg.idle_multiplier.to_string());

    // cc-switch 源 radio 组（"显示哪个 CLI 工具的当前模型"）。
    // 用 BS_AUTORADIOBUTTON + WS_GROUP 起首，OS 自动做互斥；只读 KNOWN_TOOLS。
    add_label(hwnd, hinst, label_id(ID_CCSWITCH_RADIO_BASE), "浮窗左上显示哪个 cc-switch 源:", 30, 205);
    let known = crate::cc_switch::KNOWN_TOOLS;
    let detected = crate::cc_switch::detect_available_sources();
    for (i, &tool) in known.iter().enumerate() {
        let id = ID_CCSWITCH_RADIO_BASE + i;
        let row = (i / 3) as i32;
        let col = (i % 3) as i32;
        let x = 30 + col * 130;
        let y = 228 + row * 24;
        let is_first = i == 0;
        let label = if detected.iter().any(|d| d == tool) {
            tool.to_string()
        } else {
            // 未检测到 cc-switch 该工具的 provider 配置 —— 灰色提示但仍可选
            format!("{} (未配置)", tool)
        };
        add_radio(hwnd, hinst, id, &label, x, y, is_first);
        if tool == cfg.active_cc_switch_provider {
            if let Ok(c) = GetDlgItem(Some(hwnd), id as i32) {
                SendMessageW(c, BM_SETCHECK, Some(WPARAM(BST_CHECKED.0 as usize)), Some(LPARAM(0)));
            }
        }
    }

    // 底部按钮
    add_button(hwnd, hinst, ID_OK, "确定", 280, 370);
    add_button(hwnd, hinst, ID_CANCEL, "取消", 365, 370);
    add_button(hwnd, hinst, ID_APPLY, "应用", 450, 370);

    (*dlg_ptr).hwnd = hwnd;
    (*dlg_ptr).tab_hwnd = tab;

    // 初始只显示 tab 0 的控件
    switch_tab(hwnd, 0);
    LRESULT(0)
}

unsafe fn add_tab(tab: HWND, idx: i32, label: &str) {
    let mut wtext: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    let mut ti = TCITEMW {
        mask: TCIF_TEXT,
        pszText: PWSTR(wtext.as_mut_ptr()),
        ..Default::default()
    };
    SendMessageW(
        tab,
        TCM_INSERTITEMW,
        Some(WPARAM(idx as usize)),
        Some(LPARAM(&mut ti as *mut _ as isize)),
    );
}

unsafe fn add_label(parent: HWND, hinst: HINSTANCE, id: usize, text: &str, x: i32, y: i32) {
    let w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        windows::core::w!("STATIC"),
        PCWSTR(w.as_ptr()),
        WS_CHILD,
        x, y, 190, 20,
        Some(parent),
        Some(HMENU(id as *mut _)),
        Some(hinst),
        None,
    );
}

unsafe fn add_edit(parent: HWND, hinst: HINSTANCE, id: usize, x: i32, y: i32, w: i32, initial: &str) {
    let s: Vec<u16> = initial.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        windows::core::w!("EDIT"),
        PCWSTR(s.as_ptr()),
        WS_CHILD | WS_TABSTOP | ES_AUTOHSCROLL_RAW,
        x, y, w, 22,
        Some(parent),
        Some(HMENU(id as *mut _)),
        Some(hinst),
        None,
    );
}

unsafe fn add_edit_num(parent: HWND, hinst: HINSTANCE, id: usize, x: i32, y: i32, initial: &str) {
    let s: Vec<u16> = initial.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        windows::core::w!("EDIT"),
        PCWSTR(s.as_ptr()),
        WS_CHILD | WS_TABSTOP | ES_NUMBER_RAW,
        x, y, 80, 22,
        Some(parent),
        Some(HMENU(id as *mut _)),
        Some(hinst),
        None,
    );
}

unsafe fn add_check(parent: HWND, hinst: HINSTANCE, id: usize, label: &str, x: i32, y: i32, checked: bool) {
    let s: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    let h = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        windows::core::w!("BUTTON"),
        PCWSTR(s.as_ptr()),
        WS_CHILD | WS_TABSTOP | BS_AUTOCHECKBOX_RAW,
        x, y, 400, 22,
        Some(parent),
        Some(HMENU(id as *mut _)),
        Some(hinst),
        None,
    ).unwrap_or_default();
    if checked {
        SendMessageW(h, BM_SETCHECK, Some(WPARAM(BST_CHECKED.0 as usize)), Some(LPARAM(0)));
    }
}

unsafe fn add_radio(
    parent: HWND,
    hinst: HINSTANCE,
    id: usize,
    label: &str,
    x: i32,
    y: i32,
    first_in_group: bool,
) {
    // 同一组 radio 必须连续创建，且组首 radio 设 WS_GROUP，OS 才会
    // 自动处理互斥与方向键导航。这里 first_in_group=true 时加 WS_GROUP。
    let mut style = WS_CHILD | WS_TABSTOP | BS_AUTORADIOBUTTON_RAW;
    if first_in_group {
        style |= WS_GROUP_RAW;
    }
    let s: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        windows::core::w!("BUTTON"),
        PCWSTR(s.as_ptr()),
        style,
        x, y, 125, 22,
        Some(parent),
        Some(HMENU(id as *mut _)),
        Some(hinst),
        None,
    );
}

unsafe fn add_button(parent: HWND, hinst: HINSTANCE, id: usize, label: &str, x: i32, y: i32) {
    // BS_OWNERDRAW —— 由 WM_DRAWITEM 分发到 md3::draw_button 绘制 MD3 风格
    let s: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        windows::core::w!("BUTTON"),
        PCWSTR(s.as_ptr()),
        WS_VISIBLE | WS_CHILD | WS_TABSTOP | super::md3::BS_OWNERDRAW_STYLE,
        x, y, 75, 28,
        Some(parent),
        Some(HMENU(id as *mut _)),
        Some(hinst),
        None,
    );
}

// ── Tab 切换 ────────────────────────────────────────────────────

unsafe fn switch_tab(hwnd: HWND, new_tab: usize) {
    let dlg_ptr = dialog_ptr(hwnd);
    if dlg_ptr.is_null() {
        return;
    }
    for (i, ids) in TAB_IDS.iter().enumerate() {
        let show = if i == new_tab { SW_SHOW } else { SW_HIDE };
        for &id in *ids {
            if let Ok(c) = GetDlgItem(Some(hwnd), id as i32) {
                let _ = ShowWindow(c, show);
            }
            if let Ok(l) = GetDlgItem(Some(hwnd), label_id(id) as i32) {
                let _ = ShowWindow(l, show);
            }
        }
    }
    (*dlg_ptr).current_tab = new_tab;
}

// ── 消息处理 ────────────────────────────────────────────────────

unsafe fn handle_notify(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let nmhdr = &*(lparam.0 as *const NMHDR);
    if nmhdr.idFrom == ID_TAB && nmhdr.code == TCN_SELCHANGE {
        let dlg_ptr = dialog_ptr(hwnd);
        if !dlg_ptr.is_null() {
            let sel = SendMessageW(
                (*dlg_ptr).tab_hwnd,
                TCM_GETCURSEL,
                Some(WPARAM(0)),
                Some(LPARAM(0)),
            )
            .0 as usize;
            switch_tab(hwnd, sel);
        }
    }
    LRESULT(0)
}

unsafe fn handle_command(hwnd: HWND, wparam: WPARAM, _lparam: LPARAM) -> LRESULT {
    let id = (wparam.0 as u32) & 0xFFFF;
    match id {
        x if x == ID_OK as u32 => {
            if save_and_apply(hwnd).is_ok() {
                let _ = DestroyWindow(hwnd);
            }
        }
        x if x == ID_APPLY as u32 => {
            let _ = save_and_apply(hwnd);
        }
        x if x == ID_CANCEL as u32 => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
    LRESULT(0)
}

// ── 读控件 ─────────────────────────────────────────────────────

unsafe fn get_edit_string(hwnd: HWND, id: usize) -> String {
    let Ok(ctrl) = GetDlgItem(Some(hwnd), id as i32) else {
        return String::new();
    };
    let len = GetWindowTextLengthW(ctrl) as usize + 1;
    let mut buf = vec![0u16; len];
    let read = GetWindowTextW(ctrl, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..read])
}

unsafe fn get_edit_u64(hwnd: HWND, id: usize, fallback: u64) -> u64 {
    get_edit_string(hwnd, id).trim().parse().unwrap_or(fallback)
}

unsafe fn get_edit_u32(hwnd: HWND, id: usize, fallback: u32) -> u32 {
    get_edit_string(hwnd, id).trim().parse().unwrap_or(fallback)
}

unsafe fn get_edit_usize(hwnd: HWND, id: usize, fallback: usize) -> usize {
    get_edit_string(hwnd, id).trim().parse().unwrap_or(fallback)
}

unsafe fn get_edit_f32(hwnd: HWND, id: usize, fallback: f32) -> f32 {
    get_edit_string(hwnd, id).trim().parse().unwrap_or(fallback)
}

unsafe fn get_check(hwnd: HWND, id: usize) -> bool {
    let Ok(ctrl) = GetDlgItem(Some(hwnd), id as i32) else {
        return false;
    };
    SendMessageW(ctrl, BM_GETCHECK, Some(WPARAM(0)), Some(LPARAM(0))).0 as u32 == BST_CHECKED.0
}

// ── 保存 + 应用 ─────────────────────────────────────────────────

/// 从所有控件读值 → 写 toml（保留注释）→ 应用运行态。
/// 出错时弹 MessageBox 并返回 Err 让调用方决定是否关闭对话框。
unsafe fn save_and_apply(hwnd: HWND) -> Result<(), String> {
    let dlg_ptr = dialog_ptr(hwnd);
    if dlg_ptr.is_null() {
        return Err("dialog ptr null".into());
    }
    let dlg = &*dlg_ptr;
    let cfg = &dlg.initial;

    // 读所有字段
    let new_check_interval = get_edit_u64(hwnd, ID_CHECK_INTERVAL, cfg.check_interval);
    let new_opacity = get_edit_f32(hwnd, ID_OPACITY, cfg.opacity).clamp(0.1, 1.0);
    let new_click_through = get_check(hwnd, ID_CLICK_THROUGH);
    let new_enable_log = get_check(hwnd, ID_ENABLE_LOG);

    let new_timeout = get_edit_u64(hwnd, ID_TIMEOUT, cfg.timeout);
    let new_max_retries = get_edit_u32(hwnd, ID_MAX_RETRIES, cfg.max_retries);
    let new_monitor_interval = get_edit_u64(hwnd, ID_MONITOR_INTERVAL, cfg.monitor_interval);
    let new_proxy_check_interval = get_edit_u64(hwnd, ID_PROXY_CHECK_INTERVAL, cfg.proxy_check_interval);
    let new_model_refresh_interval = get_edit_u64(hwnd, ID_MODEL_REFRESH_INTERVAL, cfg.model_refresh_interval);
    let new_proxy = get_edit_string(hwnd, ID_PROXY);
    let new_proxy_opt = if new_proxy.trim().is_empty() {
        None
    } else {
        Some(new_proxy.trim().to_string())
    };

    let new_mask_ip = get_check(hwnd, ID_MASK_IP);
    let new_mask_geo = get_check(hwnd, ID_MASK_GEO);
    let new_cross_check = get_check(hwnd, ID_CROSS_CHECK);

    let new_hotkey_toggle = get_edit_string(hwnd, ID_HOTKEY_TOGGLE);
    let new_hotkey_lookup = get_edit_string(hwnd, ID_HOTKEY_LOOKUP);
    let new_hotkey_quit = get_edit_string(hwnd, ID_HOTKEY_QUIT);

    let new_geo_cache_enabled = get_check(hwnd, ID_GEO_CACHE_ENABLED);
    let new_geo_cache_ttl = get_edit_u64(hwnd, ID_GEO_CACHE_TTL, cfg.geo_cache_ttl_hours);
    let new_geo_cache_max = get_edit_usize(hwnd, ID_GEO_CACHE_MAX, cfg.geo_cache_max_entries);
    let new_idle_threshold = get_edit_u64(hwnd, ID_IDLE_THRESHOLD, cfg.idle_threshold_seconds);
    let new_idle_multiplier = get_edit_u64(hwnd, ID_IDLE_MULTIPLIER, cfg.idle_multiplier);

    // 读 theme radio
    let new_theme = if get_check(hwnd, ID_THEME_LIGHT) {
        "light".to_string()
    } else if get_check(hwnd, ID_THEME_DARK) {
        "dark".to_string()
    } else {
        "system".to_string()
    };

    // 读 cc-switch radio：找到选中的那个，对应到 KNOWN_TOOLS 下标
    let known = crate::cc_switch::KNOWN_TOOLS;
    let mut new_cc_source = cfg.active_cc_switch_provider.clone();
    for (i, &tool) in known.iter().enumerate() {
        let id = ID_CCSWITCH_RADIO_BASE + i;
        if get_check(hwnd, id) {
            new_cc_source = tool.to_string();
            break;
        }
    }

    // 写 toml（保留注释）
    let path = config::config_path();
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: DocumentMut = raw.parse().unwrap_or_else(|_| DocumentMut::new());

    doc["check_interval"] = value(new_check_interval as i64);
    doc["opacity"] = value(new_opacity as f64);
    doc["click_through"] = value(new_click_through);
    doc["enable_log"] = value(new_enable_log);

    doc["timeout"] = value(new_timeout as i64);
    doc["max_retries"] = value(new_max_retries as i64);
    doc["monitor_interval"] = value(new_monitor_interval as i64);
    doc["proxy_check_interval"] = value(new_proxy_check_interval as i64);
    doc["model_refresh_interval"] = value(new_model_refresh_interval as i64);
    match &new_proxy_opt {
        Some(p) => doc["proxy"] = value(p.clone()),
        None => {
            doc.remove("proxy");
        }
    }

    doc["mask_ip_in_log"] = value(new_mask_ip);
    doc["mask_geo_in_log"] = value(new_mask_geo);
    doc["geo_cross_check"] = value(new_cross_check);

    doc["hotkey_toggle"] = value(new_hotkey_toggle.clone());
    doc["hotkey_lookup"] = value(new_hotkey_lookup.clone());
    doc["hotkey_quit"] = value(new_hotkey_quit.clone());

    doc["geo_cache_enabled"] = value(new_geo_cache_enabled);
    doc["geo_cache_ttl_hours"] = value(new_geo_cache_ttl as i64);
    doc["geo_cache_max_entries"] = value(new_geo_cache_max as i64);
    doc["idle_threshold_seconds"] = value(new_idle_threshold as i64);
    doc["idle_multiplier"] = value(new_idle_multiplier as i64);
    doc["active_cc_switch_provider"] = value(new_cc_source.clone());
    doc["theme"] = value(new_theme.clone());

    // 原子写盘
    let serialized = doc.to_string();
    let tmp = path.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp, &serialized) {
        show_error(hwnd, &format!("写入临时文件失败: {}", e));
        return Err(e.to_string());
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        show_error(hwnd, &format!("替换 config.toml 失败: {}", e));
        return Err(e.to_string());
    }

    // 应用到运行态 —— 立即生效的部分
    ip_fetcher::set_mask_ip_logs(new_mask_ip);
    ip_fetcher::set_mask_geo_logs(new_mask_geo);
    dlg.runtime_flags
        .geo_cache_enabled
        .store(new_geo_cache_enabled, std::sync::atomic::Ordering::Relaxed);
    dlg.runtime_flags
        .geo_cross_check
        .store(new_cross_check, std::sync::atomic::Ordering::Relaxed);

    // click_through 需要改窗口扩展样式
    if new_click_through != dlg.runtime_flags.click_through.load(std::sync::atomic::Ordering::Relaxed) {
        dlg.runtime_flags
            .click_through
            .store(new_click_through, std::sync::atomic::Ordering::Relaxed);
        super::window::set_overlay_click_through(dlg.overlay_hwnd, new_click_through);
    }

    // opacity 立即应用
    if (new_opacity - cfg.opacity).abs() > 0.001 {
        crate::gui::render::set_window_opacity(dlg.overlay_hwnd, new_opacity);
    }

    // theme 切换 → 改 RuntimeFlags 字段 + 通知主浮窗重画
    if new_theme != cfg.theme {
        if let Ok(mut g) = dlg.runtime_flags.theme_mode.write() {
            *g = new_theme.clone();
        }
        super::window::notify_theme_changed(dlg.overlay_hwnd);
    }

    // cc-switch 源切换 → 立即重读一次 label，避免等下次 model refresh tick
    if new_cc_source != cfg.active_cc_switch_provider {
        if let Ok(mut g) = dlg.runtime_flags.active_cc_switch_provider.write() {
            *g = new_cc_source.clone();
        }
        let new_label = crate::cc_switch::read_label(&new_cc_source);
        // 通过 overlay 主线程的共享 state 更新（避免直接动 GUI）
        // 简单做法：trigger ip_check_notify 不行，因为只刷 IP；改写 state.claude_model 后等下一帧 repaint。
        // 用 PostMessageW 触发主窗口 repaint。
        crate::gui::window::set_overlay_claude_label(dlg.overlay_hwnd, new_label);
    }

    // 检查"需重启"字段
    let need_restart = new_check_interval != cfg.check_interval
        || new_timeout != cfg.timeout
        || new_max_retries != cfg.max_retries
        || new_monitor_interval != cfg.monitor_interval
        || new_proxy_check_interval != cfg.proxy_check_interval
        || new_model_refresh_interval != cfg.model_refresh_interval
        || new_proxy_opt != cfg.proxy
        || new_hotkey_toggle != cfg.hotkey_toggle
        || new_hotkey_lookup != cfg.hotkey_lookup
        || new_hotkey_quit != cfg.hotkey_quit
        || new_geo_cache_ttl != cfg.geo_cache_ttl_hours
        || new_geo_cache_max != cfg.geo_cache_max_entries
        || new_idle_threshold != cfg.idle_threshold_seconds
        || new_idle_multiplier != cfg.idle_multiplier
        || new_enable_log != cfg.enable_log;

    if need_restart {
        let msg: Vec<u16> =
            "保存成功。部分字段（轮询间隔/超时/热键/缓存大小/日志开关/代理）需要重启程序后生效。"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
        let title: Vec<u16> =
            "已保存".encode_utf16().chain(std::iter::once(0)).collect();
        MessageBoxW(
            Some(hwnd),
            PCWSTR(msg.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }

    Ok(())
}

unsafe fn show_error(hwnd: HWND, msg: &str) {
    let m: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let t: Vec<u16> = "错误".encode_utf16().chain(std::iter::once(0)).collect();
    MessageBoxW(
        Some(hwnd),
        PCWSTR(m.as_ptr()),
        PCWSTR(t.as_ptr()),
        MB_OK | MB_ICONERROR,
    );
}
