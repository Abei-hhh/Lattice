//! 系统托盘图标 + 右键两层菜单。
//!
//! - 顶层：常用开关（显隐 / 锁定 / 穿透 / 掩码 IP / 掩码 Geo / 缓存 / 校验）
//! - 中层：动作（IP 查询、历史时间线）
//! - 底层：高级设置 / 打开 config / 打开日志目录 / 退出
//!
//! 所有 ☑/☐ 项**立即生效**，不写盘也不重启 —— 数值类需要重启的字段统统
//! 收纳到"高级设置..."对话框里编辑。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::runtime::RuntimeFlags;

/// Custom WM_APP message the tray icon posts back when the user clicks it.
/// lparam encodes the mouse event (WM_LBUTTONUP / WM_RBUTTONUP / etc).
pub const WM_APP_TRAY: u32 = WM_APP + 2;

const TRAY_UID: u32 = 0xCAFE_F00D;

// Menu command IDs (must not collide with hotkey or anything else routed via
// WM_COMMAND). 8000-range keeps them clear of typical control IDs.
pub const IDM_TOGGLE_VISIBLE: u32 = 8001;
pub const IDM_TOGGLE_LOCK: u32 = 8002;
pub const IDM_TOGGLE_CLICKTHROUGH: u32 = 8003;
pub const IDM_TOGGLE_MASK_IP: u32 = 8004;
pub const IDM_TOGGLE_MASK_GEO: u32 = 8005;
pub const IDM_TOGGLE_GEO_CACHE: u32 = 8006;
pub const IDM_TOGGLE_CROSS_CHECK: u32 = 8007;
pub const IDM_OPEN_CONFIG: u32 = 8011;
pub const IDM_OPEN_LOG_DIR: u32 = 8012;
pub const IDM_HISTORY: u32 = 8013;
pub const IDM_ADVANCED: u32 = 8014;
pub const IDM_USAGE_DETAIL: u32 = 8015;
// 第二行模式切换
pub const IDM_ROW2_SYSTEM: u32 = 8022;
pub const IDM_ROW2_USAGE: u32 = 8023;
/// 一键把浮窗位置恢复到当前显示器工作区顶部居中（清 overlay_state.json 里
/// 的 x/y，重新打开 auto_center），适合误拖到角落 / 多显示器拓扑变化后救场。
pub const IDM_RESET_POSITION: u32 = 8024;
pub const IDM_QUIT: u32 = 8099;

/// Register a notification icon associated with `hwnd`. The icon uses the
/// app's default IDI_APPLICATION until we wire a real .ico — adequate as a
/// placeholder and means we don't need to embed a resource yet.
pub unsafe fn register(hwnd: HWND) {
    let tip: Vec<u16> = "Vpn Monitor".encode_utf16().chain(std::iter::once(0)).collect();
    let mut tip_arr = [0u16; 128];
    for (i, c) in tip.iter().take(127).enumerate() {
        tip_arr[i] = *c;
    }

    // 优先加载嵌入的应用图标（资源 ID 1，由 build.rs 编进 exe）。
    // 加载失败（开发期没编 ico 或 ico 损坏）回退到系统默认图标。
    let hmodule = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
        .unwrap_or_default();
    let hinstance: HINSTANCE = hmodule.into();
    let hicon = LoadIconW(Some(hinstance), PCWSTR(1 as *const _))
        .or_else(|_| LoadIconW(None, IDI_APPLICATION))
        .unwrap_or_default();

    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_APP_TRAY,
        hIcon: hicon,
        szTip: tip_arr,
        ..Default::default()
    };

    if !Shell_NotifyIconW(NIM_ADD, &mut data).as_bool() {
        tracing::warn!("Shell_NotifyIconW(NIM_ADD) failed");
    }
}

pub unsafe fn unregister(hwnd: HWND) {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &mut data);
}

/// 在鼠标当前位置弹出托盘菜单。开关项的对勾从 RuntimeFlags 当场读，
/// 保证显示和实际状态同步。注意：TrackPopupMenu 前必须先 SetForegroundWindow，
/// 否则用户点击菜单外的位置不会自动收菜单。
pub unsafe fn show_menu(hwnd: HWND, flags: &Arc<RuntimeFlags>) {
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };

    let visible = flags.overlay_visible.load(Ordering::Relaxed);
    let locked = flags.overlay_locked.load(Ordering::Relaxed);
    let click_thru = flags.click_through.load(Ordering::Relaxed);
    let cache = flags.geo_cache_enabled.load(Ordering::Relaxed);
    let cross = flags.geo_cross_check.load(Ordering::Relaxed);
    let mask_ip = crate::network::ip_fetcher::get_mask_ip_logs();
    let mask_geo = crate::network::ip_fetcher::get_mask_geo_logs();

    // 读 Row 2 模式（用于子菜单 radio）
    let row2 = flags
        .row2_mode
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|p| p.into_inner().clone());
    // cc-switch 不可用时 AI 菜单项全部隐藏
    let ai_enabled = flags.cc_switch_available.load(Ordering::Relaxed);

    // ── 子菜单：显示设置（visible / lock / click-through） ──
    let display_sub = CreatePopupMenu().ok();
    if let Some(sub) = display_sub {
        add_check(sub, IDM_TOGGLE_VISIBLE, "显示浮窗", visible);
        add_check(sub, IDM_TOGGLE_LOCK, "锁定位置 (按 Shift 拖动 / 关闭会自动关穿透)", locked);
        add_check(sub, IDM_TOGGLE_CLICKTHROUGH, "鼠标穿透 (⚠ 自动锁定位置)", click_thru);
        add_sep(sub);
        add_item(sub, IDM_RESET_POSITION, "恢复默认位置");
        attach_submenu(menu, sub, "显示设置 ▸");
    }

    // ── 子菜单：第二行模式（system / usage） ──
    // AI 用量项仅 cc-switch 可用时出现；不可用时连整个子菜单都隐藏（system 是唯一选项就没意义）
    if ai_enabled {
        let row2_sub = CreatePopupMenu().ok();
        if let Some(sub) = row2_sub {
            add_check(sub, IDM_ROW2_SYSTEM, "系统资源 (↑↓/CPU/内存)", row2 == "system");
            add_check(sub, IDM_ROW2_USAGE, "AI 用量 (主模型/5h/周)", row2 == "usage");
            attach_submenu(menu, sub, "第二行模式 ▸");
        }
    }

    // ── 子菜单：隐私 & 缓存 ──
    let privacy_sub = CreatePopupMenu().ok();
    if let Some(sub) = privacy_sub {
        add_check(sub, IDM_TOGGLE_MASK_IP, "日志掩码 IP", mask_ip);
        add_check(sub, IDM_TOGGLE_MASK_GEO, "日志掩码归属地", mask_geo);
        add_check(sub, IDM_TOGGLE_GEO_CACHE, "启用归属地缓存", cache);
        add_check(sub, IDM_TOGGLE_CROSS_CHECK, "HTTPS 跨源校验", cross);
        attach_submenu(menu, sub, "隐私 & 缓存 ▸");
    }

    add_sep(menu);
    // 动作类（顶层直接可见，频繁用）
    add_item(menu, IDM_HISTORY, "历史时间线...");
    // 用量明细仅在 cc-switch 可用时出现
    if ai_enabled {
        add_item(menu, IDM_USAGE_DETAIL, "用量明细...");
    }
    add_sep(menu);
    add_item(menu, IDM_ADVANCED, "高级设置...");

    // ── 子菜单：文件 ──
    let files_sub = CreatePopupMenu().ok();
    if let Some(sub) = files_sub {
        add_item(sub, IDM_OPEN_CONFIG, "打开 config.toml");
        add_item(sub, IDM_OPEN_LOG_DIR, "打开日志目录");
        attach_submenu(menu, sub, "文件 ▸");
    }

    add_sep(menu);
    add_item(menu, IDM_QUIT, "退出");

    // The window must be foregrounded before TrackPopupMenu, otherwise the
    // menu won't dismiss when the user clicks elsewhere.
    let _ = SetForegroundWindow(hwnd);

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);

    let _ = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RIGHTALIGN,
        pt.x,
        pt.y,
        Some(0),
        hwnd,
        None,
    );

    let _ = DestroyMenu(menu);
}

unsafe fn add_item(menu: HMENU, id: u32, label: &str) {
    let w: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = AppendMenuW(menu, MF_STRING, id as usize, PCWSTR(w.as_ptr()));
}

/// 把一个 popup HMENU 挂到父菜单上作为二级子菜单（MF_POPUP）。
/// 注意：sub menu 的 HMENU 必须 cast 成 usize 作为 uIDNewItem 参数。
/// 父菜单 DestroyMenu 时会递归销毁所有子菜单，无需手动释放。
unsafe fn attach_submenu(parent: HMENU, sub: HMENU, label: &str) {
    let w: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = AppendMenuW(parent, MF_POPUP, sub.0 as usize, PCWSTR(w.as_ptr()));
}

unsafe fn add_check(menu: HMENU, id: u32, label: &str, checked: bool) {
    let flags = if checked {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING | MF_UNCHECKED
    };
    let w: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = AppendMenuW(menu, flags, id as usize, PCWSTR(w.as_ptr()));
}

unsafe fn add_sep(menu: HMENU) {
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
}

/// Open `path` in its default application via ShellExecuteW. Used by the
/// "Open config.toml" / "Open log directory" menu items.
pub fn open_external(path: &std::path::Path) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

// Needed for OsStr::encode_wide
use std::os::windows::ffi::OsStrExt;
