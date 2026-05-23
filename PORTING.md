# Vpn_Monitor 多平台适配文档

> 目标：把 Windows 独占的 Vpn_Monitor 演进为 Windows / Linux / macOS / Android / iOS 五端可用。
> 本文给出**现状审计 → 分层重构 → 各平台路径 → 选型对比 → 工作量预估 → 推荐路线图**。

## TL;DR

| 平台 | 浮窗可行性 | 推荐 UI 方案 | 难度 |
|---|---|---|---|
| **Windows** | ✅ 已实现 | 保留 Win32 | — |
| **Linux** | ✅（X11 容易、Wayland 受限） | egui / Slint | ★★ |
| **macOS** | ✅（NSWindow level 28） | egui / AppKit FFI | ★★ |
| **Android** | ⚠️ 需 SYSTEM_ALERT_WINDOW 权限，部分 OEM 阻断 | Compose + Rust JNI + 前台服务 | ★★★★ |
| **iOS** | ❌ 系统禁止悬浮窗 | 改用 Live Activity / Widget / 应用内首屏 | ★★★★★ |

**核心建议**：拆出 `vpn-monitor-core` 共享 Rust crate（网络 / 缓存 / cc-switch / 配置），各平台只写 UI 壳。桌面三端用同一套 egui，移动端用平台原生 UI 调 Rust core。

---

## 1. 现状审计

### 1.1 当前依赖的 Windows 独占 API

| 模块 | Win32 调用 | 跨平台等价 |
|---|---|---|
| `gui/window.rs` | `CreateWindowExA`, `WS_POPUP`, `WS_EX_LAYERED`, `SetWindowPos(HWND_TOPMOST)`, `WM_NCHITTEST`, `WM_POWERBROADCAST` | 各平台 native window API |
| `gui/render.rs` | GDI（`CreateSolidBrush`, `DrawTextW`, `RoundRect`） | egui / cairo / Quartz |
| `gui/tray.rs` | `Shell_NotifyIconW` | macOS `NSStatusItem`、Linux `tray-icon` crate、移动端无 |
| `gui/hotkey.rs` | `RegisterHotKey` 全局热键 | Linux X11 `XGrabKey`、macOS `RegisterEventHotKey`、移动端无 |
| `gui/theme.rs` | HKCU 注册表读 AppsUseLightTheme、`DwmSetWindowAttribute` | macOS `NSApp.effectiveAppearance`、Linux `gsettings org.gnome.desktop.interface color-scheme` |
| `gui/overlay_state.rs` | `dirs::data_dir()` (Win 用 `%APPDATA%`) | dirs crate 已跨平台 ✅ |
| `gui/lookup_dialog.rs` / `history_dialog.rs` / `settings_dialog.rs` | Win32 dialog + ListView + TabControl | egui widgets |
| `monitor.rs` | 注册表 `ProxyEnable` / `AutoConfigURL`、`GetLastInputInfo` | 各平台不同（详见 §3） |
| `main.rs` 单实例 | 命名 mutex `CreateMutexW` | Linux: lock file + flock；macOS: 同 Linux 或 NSDistributedNotificationCenter |
| `build.rs` | `embed-resource` 编 .rc | Linux/macOS 不需要资源段，图标走 .desktop 或 .icns |

### 1.2 已经跨平台 OK 的部分（无需改）

| 模块 | 说明 |
|---|---|
| `network/ip_fetcher.rs` | `reqwest` + `tokio`，全平台 ✅ |
| `network/geo_lookup.rs` | 同上 ✅ |
| `network/geo_cache.rs` | `std::fs` + `dirs`，路径会自动用平台约定（Linux `~/.local/share/`，macOS `~/Library/Application Support/`） ✅ |
| `config.rs` | `toml` + `serde` ✅ |
| `cc_switch.rs` | 读 `~/.cc-switch/settings.json`，前提：cc-switch 本身在该平台可用 |
| `runtime.rs` | 纯 atomic / RwLock ✅ |
| `sysinfo` crate | 已支持 Win/Linux/macOS/Android/iOS ✅ |

**关键洞察**：约 **50%** 代码（网络 / 缓存 / 配置 / 监控数据采集）已经跨平台，可以无改动复用。重写工作量集中在 GUI 层。

---

## 2. 分层重构方案

### 2.1 目标 workspace 结构

```
vpn-monitor/
├── crates/
│   ├── core/              # 平台无关：网络、缓存、配置、cc-switch、runtime flags
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── ip_fetcher.rs
│   │   │   ├── geo_lookup.rs
│   │   │   ├── geo_cache.rs
│   │   │   ├── cc_switch.rs
│   │   │   ├── config.rs
│   │   │   └── runtime.rs
│   │   └── Cargo.toml
│   │
│   ├── platform/          # 平台抽象 trait：proxy 检测 / 单实例 / 主题探测 / 空闲探测
│   │   ├── src/
│   │   │   ├── lib.rs     # pub trait Platform { fn detect_proxy() -> ProxyState; ... }
│   │   │   ├── windows.rs # #[cfg(target_os = "windows")] 当前 monitor.rs / window.rs 里 Win32 部分
│   │   │   ├── linux.rs   # X11 / Wayland 各自分支
│   │   │   ├── macos.rs   # objc / system_configuration
│   │   │   ├── android.rs # JNI 调 Android API
│   │   │   └── ios.rs     # 极简实现，只能查 IP 不能查 proxy
│   │   └── Cargo.toml
│   │
│   └── monitor-app/       # 桌面端三平台共用 GUI（egui）
│       ├── src/
│       └── Cargo.toml
│
├── apps/
│   ├── desktop/           # Windows / Linux / macOS 入口（main.rs，调 monitor-app）
│   ├── android/           # Android Studio 项目，JNI bridge 调 core
│   └── ios/               # Xcode 项目，Swift Package 调 core via uniffi / 手写 FFI
│
└── Cargo.toml             # workspace
```

### 2.2 `core` crate 接口设计

```rust
// crates/core/src/lib.rs

pub use ip_fetcher::*;
pub use geo_lookup::*;
pub use geo_cache::*;
pub use cc_switch::*;
pub use config::*;
pub use runtime::*;

/// 上层（egui / Compose / SwiftUI）订阅这个 stream 拿状态推送
pub struct AppCore {
    pub state: Arc<Mutex<OverlayState>>,
    pub flags: Arc<RuntimeFlags>,
    pub notify: Arc<tokio::sync::Notify>,
}

impl AppCore {
    pub async fn new(config: AppConfig) -> Self { ... }
    pub async fn run(&self, platform: Arc<dyn Platform>) { ... }
    pub async fn snapshot(&self) -> OverlayState { ... }
}
```

### 2.3 `platform` crate trait

```rust
// crates/platform/src/lib.rs

pub trait Platform: Send + Sync {
    fn detect_proxy(&self) -> ProxyState;
    fn user_idle_seconds(&self) -> u64;
    fn theme_mode_is_dark(&self) -> bool;
    fn acquire_single_instance(&self) -> Result<SingleInstanceGuard, AlreadyRunning>;
    fn open_external(&self, path: &Path);
}

#[cfg(target_os = "windows")]
pub fn current() -> Arc<dyn Platform> { Arc::new(windows::WindowsPlatform::new()) }

#[cfg(target_os = "linux")]
pub fn current() -> Arc<dyn Platform> { Arc::new(linux::LinuxPlatform::new()) }

// 等等
```

---

## 3. 各平台具体实施

### 3.1 Linux

#### 浮窗可行性
- **X11**：完全可行。用 `_NET_WM_WINDOW_TYPE_DOCK` + `_NET_WM_STATE_ABOVE` 实现常驻置顶不抢焦点。`xcb` / `x11rb` crate 可直接调
- **Wayland**：受限。原生 Wayland 无"全屏置顶"概念，需 compositor 支持 `wlr-layer-shell` 协议（KDE / Sway / Hyprland 支持，GNOME 不支持）。**GNOME 用户回退到任务栏图标或托盘**

#### 推荐技术栈
- GUI：**egui + winit**（透明窗口 + always-on-top 支持，跨 X11/Wayland）
- 备选：**Slint**（更声明式，dark/light 主题原生支持）
- 托盘：[`tray-icon`](https://crates.io/crates/tray-icon) crate，桥接 freedesktop StatusNotifierItem + 老 XEmbed
- 全局热键：[`global-hotkey`](https://crates.io/crates/global-hotkey) crate（X11 直接调，Wayland 需 portal）

#### 平台特有事项
| 项 | 实现 |
|---|---|
| 代理检测 | 读 `$http_proxy` / `$HTTPS_PROXY` 环境变量 + GNOME `gsettings get org.gnome.system.proxy mode` + KDE `~/.config/kioslaverc` |
| 主题探测 | GNOME: `gsettings get org.gnome.desktop.interface color-scheme`；KDE: `~/.config/kdeglobals`；通用：`xdg-portal` color scheme |
| 用户空闲 | X11: `XScreenSaverQueryInfo`；Wayland: `org.gnome.Mutter.IdleMonitor` D-Bus |
| 单实例 | `flock` `~/.local/share/vpn-monitor/single.lock` |
| 图标 | `.desktop` + `~/.local/share/icons/hicolor/256x256/apps/vpn-monitor.png` |
| 自启 | `~/.config/autostart/vpn-monitor.desktop` |
| 打包 | AppImage（推荐）/ Flatpak / .deb / .rpm |

#### 工作量
- 拆 core / platform crate：3 天
- Linux platform 实现：5 天
- egui UI 移植：1 周（浮窗 + 设置 + 历史三窗）
- 打包 + 测试：2 天
- **小计：约 3 周**

---

### 3.2 macOS

#### 浮窗可行性
完全可行，比 Windows 还干净：
- `NSWindow.level = .floating` 或 `.statusBar` 实现置顶
- `NSWindow.collectionBehavior = [.canJoinAllSpaces, .stationary]` 跨 Space 常驻
- 透明：`NSWindow.isOpaque = false` + `backgroundColor = .clear`
- 鼠标穿透：`ignoresMouseEvents = true`

#### 推荐技术栈
- **egui + winit**（跨平台首选；winit 0.30 macOS 支持成熟）
- 备选：`objc2` + `AppKit` 手写（最佳原生体验，工作量翻倍）
- 菜单栏图标：`NSStatusBar` via `objc2-app-kit`（推荐）或 `tray-icon` crate
- 全局热键：`global-hotkey` crate（macOS 用 `RegisterEventHotKey`）

#### 平台特有事项
| 项 | 实现 |
|---|---|
| 代理检测 | `scutil --proxy`、或 SystemConfiguration framework `SCDynamicStoreCopyProxies` |
| 主题探测 | `NSApp.effectiveAppearance == .darkAqua` 或读 `defaults read -g AppleInterfaceStyle` |
| 用户空闲 | `CGEventSource.secondsSinceLastEventType(.combinedSessionState, .any)` |
| 单实例 | `NSRunningApplication` 检查 + lock file 兜底 |
| 应用打包 | `.app` bundle，里面 Info.plist 标 `LSUIElement = true` 让 dock 不显示 |
| 图标 | `.icns`（iconutil 从 256/512 PNG 生成） |
| 自启 | `LaunchAgent` plist 投 `~/Library/LaunchAgents/` |
| 公证 / 签名 | Apple Developer ID（$99/年），否则 Gatekeeper 拦下 |

#### 工作量
- macOS platform 实现：5 天
- egui UI（与 Linux 共用）：复用，~2 天调样
- .app 打包 + 签名：3 天
- **小计：约 2 周（在 Linux 已完成情况下）**

---

### 3.3 Android

#### 浮窗可行性 ⚠️
- 技术上可行：`WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY` + `SYSTEM_ALERT_WINDOW` 权限
- **但**：Android 12+ 起，系统对悬浮窗大幅收紧
  - 用户必须手动在系统设置开启权限（无法在 manifest 中预授权）
  - 部分 OEM（MIUI / EMUI / Flyme）有自己的"悬浮窗管理"叠加在系统之上，可能直接禁用
  - 后台服务唤醒受 Doze / App Standby Buckets 严格限制
  - 国内厂商进一步限制：默认杀后台、限制网络访问

#### 现实方案
**首选**：常驻**前台通知**展示 IP / 状态，不做悬浮窗。
- `ForegroundService` + `NotificationCompat.Builder` 自定义大图样式
- 通知抽屉里像 widget 一样实时刷新
- 用户体验类似 PowerToys Awake 在通知栏的显示
- 不需要 SYSTEM_ALERT_WINDOW 权限，过审麻烦少很多

**次选**：浮窗 + App 内主屏共存
- App 内主屏 = 完整功能（设置、历史等）
- 浮窗 = 可选高级功能，需用户主动赋权
- 权限被拒绝时优雅回退到通知栏

#### 推荐技术栈
- **UI**：Jetpack Compose（声明式，类似 SwiftUI）
- **Rust 集成**：[uniffi](https://github.com/mozilla/uniffi-rs) 生成 Kotlin binding，或手写 JNI
- **后台**：`ForegroundService` + Compose for Notifications
- **构建**：Android Studio + cargo-ndk 编 .so 嵌入 APK
- **图标**：Adaptive Icon (background + foreground layer)

#### 平台特有事项
| 项 | 实现 |
|---|---|
| 代理检测 | `ConnectivityManager.defaultProxy` |
| 主题探测 | `Configuration.uiMode & UI_MODE_NIGHT_MASK` |
| 用户空闲 | 不存在精确 API，可用屏幕 ON/OFF 状态近似 |
| 单实例 | Activity launchMode = singleTask |
| 网络后台 | Foreground Service + `FOREGROUND_SERVICE_DATA_SYNC` permission |
| 全局热键 | **不存在**，移除或映射为通知 action |
| cc-switch 集成 | cc-switch 是桌面工具，移动端通常没有 `~/.cc-switch/` —— 隐藏该功能 |
| 商店上架 | Google Play 对前台服务有严格类别要求，需声明数据用途 |

#### 工作量
- Rust core 编译为 Android .so：3 天
- uniffi 生成 binding 或手写 JNI：3 天
- Compose UI（主屏 + 设置 + 历史）：2 周
- Foreground Service + 通知样式：5 天
- 浮窗（可选，受权限限制）：1 周
- 国内厂商兼容测试 + 适配：1 周
- **小计：约 6-8 周**

---

### 3.4 iOS

#### 浮窗可行性 ❌
**Apple 系统层禁止**第三方 App 在其他 App 之上画 UI（PIP 仅限视频）。这条死路。

#### 现实替代方案

| 替代 | 说明 |
|---|---|
| **Live Activity**（推荐） | iOS 16.1+，锁屏 + Dynamic Island 实时显示。可显示当前 IP / 国家。每 ~1 小时只能更新约 4 次（系统限制），且 App 必须在前台或后台才能 push 更新 |
| **Home Screen Widget** | 桌面小组件，时间线刷新（一般 5-15 分钟一次）。不能实时但低成本 |
| **App 内主屏** | 完整功能在 App 内展示。打开 App 即看到所有信息 |
| **本地推送通知** | IP 变化时推一条通知到通知中心 |

#### 推荐技术栈
- **UI**：SwiftUI（iOS 14+）
- **Rust 集成**：[swift-bridge](https://github.com/chinedufn/swift-bridge) 或 uniffi 生成 Swift bindings
- **后台执行**：仅在用户允许"VPN App 类别"或"位置/网络变化"时获得有限后台时间
- **构建**：Xcode + cargo-lipo 编 universal binary
- **图标**：1024×1024 png → Xcode asset catalog

#### 限制现实
- **没有持续后台运行**：除非加 Background Modes（VPN、Location、Audio 之一），否则 App 进后台后几分钟就被系统挂起
  - 这意味着"每 10 秒查一次 IP"在 iOS 上**不可能**实现
  - 实际能做的：用户打开 App 时刷一次 / Background Fetch（系统决定何时拉，间隔不可控） / Live Activity 推更新
- **没有全局热键 / 托盘 / 文件系统访问**：cc-switch 集成、地理缓存导出 CSV 等功能都不适用
- **必须有 $99/年 Apple Developer 账户**才能装到真机和上架

#### 工作量
- Rust core 编译为 iOS .a：3 天
- swift-bridge / uniffi 集成：5 天
- SwiftUI App 主屏：1 周
- Home Screen Widget：3 天
- Live Activity：5 天
- App Store 上架材料 / 隐私清单 / 审核：1 周
- **小计：约 4-5 周（实现的功能比桌面端少很多）**

---

## 4. GUI 框架选型对比

| 框架 | Windows | Linux | macOS | Android | iOS | 浮窗能力 | 透明度 | 体积影响 | 评价 |
|---|---|---|---|---|---|---|---|---|---|
| **Win32 直调**（现状） | ✅ | ❌ | ❌ | ❌ | ❌ | 完美 | 完美 | 最小 | 已实现，无视 |
| **egui + winit** | ✅ | ✅ | ✅ | ⚠️ 实验性 | ⚠️ 实验性 | 好（winit 支持） | 好 | +1-2MB | **桌面端首选** |
| **Slint** | ✅ | ✅ | ✅ | ✅ | ✅ | 中（需配置） | 好 | +2-3MB | 移动端也可，但 1.0 还在打磨 |
| **iced** | ✅ | ✅ | ✅ | ⚠️ alpha | ❌ | 中 | 中 | +2MB | 仅桌面 |
| **Dioxus 0.5+** | ✅ | ✅ | ✅ | ✅ | ✅ | 弱（基于 webview） | 弱 | +20MB+ | 跨度最大但不适合浮窗 |
| **Tauri 2** | ✅ | ✅ | ✅ | ✅ | ✅ | 弱（webview） | 弱 | +10-20MB | 同上 |
| **gpui (Zed)** | ✅ | ✅ | ✅ | ❌ | ❌ | 好 | 好 | +5MB | 仅桌面，文档少，未稳定 |
| **Native 各端** | Win32 | GTK4/Qt | AppKit | Compose | SwiftUI | 完美 | 完美 | 最小 | UI 工作量 ×5 |

**推荐组合**：
- 桌面三端：**egui + winit** —— 单代码库，浮窗 + 透明都支持
- 移动两端：**原生**（Compose / SwiftUI）+ uniffi 调 Rust core

---

## 5. 工作量预估汇总

| 阶段 | 工作量 | 累计 |
|---|---|---|
| 拆 workspace / 提取 core crate | 1 周 | 1 周 |
| platform abstraction trait + Windows 实现回填（确保不退化） | 1 周 | 2 周 |
| egui 桌面 UI（覆盖 Win/Linux/macOS） | 2-3 周 | 5 周 |
| Linux platform 实现 + 打包 | 1.5 周 | 6.5 周 |
| macOS platform 实现 + 签名打包 | 1.5 周 | 8 周 |
| Android（前台通知方案，不含悬浮窗） | 6 周 | 14 周 |
| iOS（主屏 + Widget + Live Activity） | 4-5 周 | 19 周 |
| 全端集成测试 + 文档 + CI | 2 周 | **21 周（约 5 个月单人全职）** |

**保守估计**：1 人全职 5-6 个月；3 人并行约 2-3 个月。

---

## 6. 推荐路线图

### Phase 0：Workspace 重构（必经）✅ 已完成

`crates/core/` 子包已抽出，包含 `network/`、`config.rs`、`cc_switch.rs`、`runtime.rs` —— 这些模块在 Linux/macOS 上**无需任何修改**即可编译运行（仅靠 `reqwest`/`tokio`/`dirs`/`serde` 等已跨平台的 crate）。

binary crate 通过 `pub use vpn_monitor_core::{cc_switch, config, network, runtime};` 做路径 alias，旧 `crate::*` 引用全部不变。后续在 binary 内逐步迁移到直接 `vpn_monitor_core::*` 形式。

**platform trait 抽象**（监控 / 单实例 / 主题探测 / 空闲探测）留给 Phase 0.5：等 Linux 实施时同步落地，避免空抽象。

### Phase 1：Linux 桌面端
Linux 用户群和 Windows 互补，且技术风险最低（egui + X11 路径成熟）。**3 周**。

### Phase 2：macOS 桌面端
复用 egui 代码，只补 platform trait 的 macOS 实现 + .app 打包 + 签名。**2 周**。

🎯 **里程碑：桌面三端完成**（约 2 个月），覆盖 80%+ 目标用户。

### Phase 3：Android 前台通知版
**不做悬浮窗，做前台通知**。功能阉割：去掉 cc-switch 集成、全局热键、托盘。保留 IP/Geo 监控、设置、历史。**6 周**。

### Phase 4：iOS Live Activity 版
功能最阉割版本：App 内查 IP + Widget + 锁屏 Live Activity。**4-5 周**。

---

## 7. 关键技术风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| Wayland 浮窗碎片化 | Linux 一部分用户无法用浮窗 | 优雅降级到托盘 + 通知 |
| Android 国内 OEM 杀后台 | 数据不刷新 | 引导用户加白名单 + 提示 |
| iOS 后台执行受限 | 无法实时刷新 IP | 用户打开 App 时刷新 + Live Activity push（频率受限） |
| Apple 审核拒绝 | App 无法上架 | 强调"网络状态展示"非"VPN 控制"，避开敏感类目 |
| cc-switch 跨平台 | 移动端没有 ~/.cc-switch/ | 该模块仅在桌面三端启用，移动端隐藏 UI |
| 单一 Rust core 维护多平台 cfg | 代码可读性下降 | platform crate 严格 cfg(target_os) 隔离；core 完全平台无关 |

---

## 8. 后续可考虑的差异化

桌面三端做完后，可针对各平台做差异化：

| 平台 | 增值功能 |
|---|---|
| Windows | 已有：Win11 Mica 材质背景、Tiles 集成 |
| Linux | Hyprland / Sway 用户的 wlr-layer-shell 真悬浮 |
| macOS | Touch Bar 显示 IP（已废弃硬件，可跳）、Dynamic Island-style 菜单栏动画 |
| Android | Tasker 集成（IP 变化触发自定义脚本） |
| iOS | Shortcuts.app 集成（"我现在哪个国家"语音查询） |

---

## 9. 参考资料

- [winit docs - transparent windows](https://docs.rs/winit/latest/winit/window/struct.WindowAttributes.html#method.with_transparent)
- [egui examples - custom window](https://github.com/emilk/egui/tree/master/examples/custom_window_frame)
- [Apple HIG - Live Activities](https://developer.apple.com/design/human-interface-guidelines/live-activities)
- [Android 12 Restrictions on starting activities from the background](https://developer.android.com/guide/components/activities/background-starts)
- [uniffi-rs guide](https://mozilla.github.io/uniffi-rs/)
- [tray-icon crate](https://github.com/tauri-apps/tray-icon)
- [global-hotkey crate](https://github.com/tauri-apps/global-hotkey)

---

## 10. Roadmap：功能创新池（按维度分组）

下列是之前讨论中累积的差异化方向，从已实现到候选未实现按"做过 → 一梯队 → 二梯队 → 三梯队"排序。**桌面跨平台落地后再考虑接入**。

### ✅ 已落地
- 浮窗稳定性三连：唤醒即时重查、代理变化联动、监控线程 panic 自动重启
- Geo 缓存 /24 归并 + LRU + TTL
- HTTPS 跨源校验 + UI ⚠ 警告（抵御 HTTP MITM 伪造归属地）
- 日志 IP / 归属地 / 代理凭证三类脱敏
- 托盘菜单多层（含浮窗形态 / row2 模式 radio） + 高级设置 5-Tab 对话框 + 历史时间线窗口
- 拖动 / 锁定 / 位置记忆（独立 overlay_state.json）
- 应用图标编译期 SVG → ICO → embed-resource
- 主题系统（system / light / dark）+ MD3 owner-draw 按钮
- cc-switch 多源（Claude / Codex / Gemini / OpenCode / Hermes / OpenClaw）
- **cc-switch SQLite 用量统计**（5h / 本周）+ 浮窗双形态（简易 / 完整 + 流量曲线 + 国家分布）
- **DNS + IPv6 泄漏检测**（v6 IP 查国别 + Cloudflare `/cdn-cgi/trace` 拿 DNS 解析位置）
- **Clash / Mihomo / sing-box RPC 集成**（自动探测 9090/9001/6170，浮窗显示节点名）
- **流量分流可视化**（GetExtendedTcpTable + 国别堆叠条）
- **用量明细窗口**（4 时段 radio + provider×model 分组列表，按 cost 降序）
- **Workspace 拆分**（Phase 0）：core 子 crate 跨平台编译验证通过

### 🥇 第一梯队（基础设施已有，做起来事半功倍）

#### ASN + AS 名展示（零成本）
ip-api.com 响应里已有 `as`/`asname` 字段（如 `AS13335 Cloudflare`），目前丢了不用：
- `GeoInfo` 加 `asn: String` 字段
- 浮窗第二行（system 模式）显示 `AS13335 (Cloudflare)`
- ASN 比 ISP 名稳定多了，对网络/安全人员极有用

实施位置：`crates/core/src/network/geo_lookup.rs` 加字段；render.rs 显示

#### Sparkline hover 高亮态
当前 detailed sparkline 是被动展示。可加：
- 鼠标 hover 显示对应时间点的精确速率值（tooltip 风格小气泡）
- 需要给浮窗加 WM_MOUSEMOVE 跟踪（注意与拖动 HTCAPTION 冲突，可能要细化 hit-test）

### 🥈 第二梯队（需要更多基础设施）

#### MCP server 暴露状态
做个 stdio MCP server，把当前 IP/地理/代理/用量状态暴露给 Claude Code：
- "我现在哪个国家？" → 调 MCP
- "今天切过几次节点？" → 读历史缓存
- "5h 用量花了多少？" → 读 cc-switch SQLite
- 和 cc-switch 集成同源思路

实施位置：新建 `apps/mcp-server/` 子 crate（workspace 第三个 member）

#### 本地 GeoLite2 mmdb 作为第三票 + 离线兜底
当前在线两 provider + cache hits 已覆盖 80%+ 场景，但仍有死角：
- 离线 / 内网环境无法查 → 浮窗一直空白
- TCP 表里的国家分布只能 cache hits → 大部分新连接归"未知"
- mmdb 一并解决两个问题

具体计划：
- 三票交叉：在线两家 + mmdb，三家一致 100% 放心；mmdb 与在线均不一致 → 黄色告警（GeoIP 数据投毒 / 缓存滞后）
- TCP 流量分流：未命中 cache 的 IP fallback 到 mmdb 查国家 → 国别分布从 "30% 未知" 降到 "5% 未知"
- ~70MB 可选下载（"安装增强包"），首次启动提示用户

实施位置：`crates/core/src/network/geo_mmdb.rs`，依赖 `maxminddb`

### 🥉 第三梯队（独立小功能）

#### 节点冷启动测速
切到新代理节点（IP 变化时触发），并发 ping/HTTP 多目标，3s 出"新节点：优/一般/差"提示：
- IP 变化的钩子已存在（`ip_changed = true`）
- 测速目标：Google/GitHub/YouTube/Cloudflare 4 个并发
- 浮窗弹一次 toast / 简短动画

实施位置：`crates/core/src/network/speed_test.rs`

#### 告警通知
落地特定国家（"白名单外"）时弹 toast：
- `tracing-subscriber` 加 filter layer，触发 WinToast
- 用户配 expected_countries = ["US", "JP"]，不在列表里弹通知
- 出墙 / 反向出墙都告警

实施位置：`src/gui/toast.rs`，依赖 `winrt-notification` 或手写 ToastNotificationManager

#### Tasker / Shortcuts 集成
- Windows: 暴露 PowerShell module（`Get-VpnMonitorState`）
- macOS: 注册 Shortcuts action "What's my IP?"
- Android: Tasker plugin（IP 变化 → 触发任意脚本）

### 📋 风险池（暂不推荐）

- **节点延迟时序数据库**（InfluxDB / Prometheus）：体积大，受众窄，不如直接 Grafana 接 PowerShell 拉
- **VPN 节点自动切换**：超出工具定位（变成 Clash 上层而非监控）
- **抓包 / 流量分析**：需要 admin 权限 + WinPcap/Npcap，复杂度爆炸

---

## 11. 总结



| 维度 | 结论 |
|---|---|
| 跨平台核心**可行** | 50% 代码已经跨平台，剩下要重写的 GUI 层是常规工作量 |
| 桌面三端**性价比高** | 共用 egui 代码，约 2 个月单人全职完成 |
| 移动端**功能不对等** | iOS 无法做浮窗，Android 部分受限，需要重新设计 UX |
| 重构必须**先拆 core** | 否则平台分支会污染所有模块，越往后越难拆 |
| **不推荐路线**：Tauri / Dioxus 这类 webview 方案；浮窗体验差、体积涨 10 倍以上、CPU 占用大 |
