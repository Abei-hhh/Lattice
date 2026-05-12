# Vpn_Monitor - Windows IP 状态悬浮窗应用设计文档

## 1. 项目概述

一个运行在 Windows 11 上的纯后台 Rust 应用，以顶部悬浮条形式实时显示当前公网 IP 及其归属地信息。无系统托盘图标、无右键菜单、无任务栏入口，仅在任务管理器"详细信息"页中可见。通过全局快捷键控制悬浮窗显隐和 IP 查询工具。

### 核心目标

- **始终可见**：屏幕顶部半透明悬浮条，不干扰正常操作，可通过快捷键切换显隐
- **自动检测**：后台定时轮询，IP 变化时即时更新
- **全球归属地**：支持全球任意 IP 的归属地查询（国家/地区/城市/ISP）
- **低资源占用**：后台常驻内存 < 30MB，CPU 空闲时接近 0%
- **离线容错**：网络中断时显示"网络不可达"，恢复后自动恢复
- **纯后台运行**：无托盘图标、无任务栏窗口、无右键菜单，仅通过快捷键交互

---

## 2. 技术选型

| 组件 | 选型 | 理由 |
|------|------|------|
| 语言 | Rust | 性能、内存安全、适合常驻后台 |
| GUI 框架 | `windows-rs` (windows crate) | 原生 Win32 API，零额外依赖，窗口样式完全可控 |
| HTTP 客户端 | `reqwest` (rustls) | 纯 Rust TLS，无 OpenSSL 依赖 |
| IP 查询 API | `ip-api.com` / `ipwho.is` | 免费、无需 Key、JSON 响应、全球覆盖 |
| 异步运行时 | `tokio` | 成熟稳定，适合定时任务 + HTTP 请求 |
| 配置管理 | `serde` + TOML | 简洁的人类可读配置文件 |
| 全局快捷键 | `windows-rs` RegisterHotKey | 原生 Win32 全局热键，无需额外依赖 |

### 为什么不选 WebView / Tauri / egui

- **Tauri/WebView2**：引入浏览器引擎，内存 > 80MB，杀鸡用牛刀
- **egui**：偏游戏/工具场景，悬浮窗 + 点击穿透支持不如原生 Win32
- **原生 Win32**：悬浮窗只需 `CreateWindowExW` + `WS_EX_LAYERED`，内存极低

---

## 3. 架构设计

```
┌──────────────────────────────────────────────────────┐
│                     main.rs (入口)                     │
│         初始化 → 注册快捷键 → 启动 GUI → 启动轮询      │
└──────────┬───────────────────────┬────────────────────┘
           │                       │
           ▼                       ▼
┌───────────────────┐   ┌─────────────────────────────┐
│   gui/ 模块        │   │   network/ 模块               │
│                   │   │                             │
│  ┌─────────────┐  │   │  ┌─────────────────────┐   │
│  │ 悬浮窗口     │  │   │  │ IP 轮询任务          │   │
│  │ (TOPMOST)    │  │   │  │ (tokio interval)    │   │
│  └──────┬──────┘  │   │  └──────────┬──────────┘   │
│         │         │   │             │               │
│  ┌──────▼──────┐  │   │  ┌──────────▼──────────┐   │
│  │ 渲染引擎     │  │   │  │ IP 查询客户端        │   │
│  │ (Direct2D)  │  │   │  │ (reqwest)           │   │
│  └──────┬──────┘  │   │  └──────────┬──────────┘   │
│         │         │   │             │               │
│  ┌──────▼──────┐  │   │  ┌──────────▼──────────┐   │
│  │ 快捷键管理   │  │   │  │ 归属地查询           │   │
│  │ (HotKey)    │  │   │  │ (全球 IP 支持)       │   │
│  └──────┬──────┘  │   │  └─────────────────────┘   │
│         │         │   │                             │
│  ┌──────▼──────┐  │   │                             │
│  │ IP 查询窗口  │  │   │                             │
│  │ (工具窗口)   │  │   │                             │
│  └─────────────┘  │   │                             │
└───────────────────┘   └─────────────────────────────┘
           │                       │
           ▼                       ▼
┌──────────────────────────────────────────────────────┐
│               消息通道 (tokio mpsc)                   │
│          network → gui (IpUpdate 消息)                │
└──────────────────────────────────────────────────────┘
```

### 3.1 模块划分

```
src/
├── main.rs            # 入口：解析配置、注册全局快捷键、启动 runtime、启动 GUI
├── config.rs          # 配置结构体 + TOML 加载
├── gui/
│   ├── mod.rs         # GUI 模块入口
│   ├── window.rs      # 悬浮窗口创建 & 消息循环
│   ├── render.rs      # Direct2D 文字渲染
│   ├── hotkey.rs      # 全局快捷键注册与处理
│   └── lookup_dialog.rs  # IP 查询工具窗口（输入 IP → 显示归属地）
└── network/
    ├── mod.rs         # network 模块入口
    ├── ip_fetcher.rs  # 公网 IP 获取 (多源备份)
    └── geo_lookup.rs  # IP 归属地查询 (全球支持)
```

---

## 4. 核心流程

### 4.1 启动流程

```
main()
 ├── 解析命令行参数（可选配置文件路径）
 ├── 加载 config.toml（不存在则使用默认值）
 ├── 创建 tokio runtime
 ├── 创建 mpsc channel <IpUpdate>
 ├── 注册全局快捷键
 │    ├── Ctrl+Alt+I → 打开 IP 查询工具窗口
 │    └── Ctrl+Alt+H → 切换悬浮窗显示/隐藏
 ├── spawn IP 轮询异步任务
 │    └── loop { fetch_ip() → geo_lookup() → send(IpUpdate) → sleep(interval) }
 └── 运行 GUI 消息循环（当前线程）
      ├── 创建悬浮窗口（TOPMOST + LAYERED）
      ├── 处理 WM_HOTKEY 消息（快捷键响应）
      └── loop { GetMessage → 处理自定义消息（更新 IP 显示）→ DispatchMessage }
```

### 4.2 IP 检测流程

```
┌─────────────┐
│ 定时器触发    │
└──────┬──────┘
       ▼
┌─────────────────────┐
│ 并行请求多源 IP      │  ipify.org / ip.sb / ifconfig.me
│ (取最先返回的结果)    │
└──────┬──────────────┘
       ▼
┌─────────────────────┐     ┌──────────────┐
│ IP 是否变化?         │──否→│ 无操作，等待下次 │
└──────┬──────────────┘     └──────────────┘
       │ 是
       ▼
┌─────────────────────┐
│ 查询归属地信息        │  ip-api.com/json/{ip}
│ (国家/省/市/ISP)      │
└──────┬──────────────┘
       ▼
┌─────────────────────┐
│ 发送 IpUpdate 到 GUI │
│ (通过 mpsc channel)  │
└─────────────────────┘
```

### 4.3 窗口渲染流程

```
收到 WM_PAINT / 自定义 WM_UPDATE_IP
 │
 ├── 创建 Direct2D RenderTarget
 ├── Clear 背景 (半透明暗色)
 ├── 绘制 IP 文本 (白色，等宽字体)
 ├── 绘制归属地文本 (浅灰，较小字号)
 ├── 绘制状态指示灯 (🟢已连接 / 🔴离线)
 └── 交换缓冲区
```

---

## 5. API 接口设计

### 5.1 IP 获取（多源备份，取最快返回）

| 优先级 | API | 响应格式 | 说明 |
|--------|-----|---------|------|
| 1 | `https://api.ipify.org` | 纯文本 IP | 最快最稳定 |
| 2 | `https://api.ip.sb/ip` | 纯文本 IP | 备用 |
| 3 | `https://ifconfig.me/ip` | 纯文本 IP | 备用 |

### 5.2 IP 归属地查询

**主用**: `http://ip-api.com/json/{ip}?lang=zh-CN`

支持全球所有国家的 IP 归属地查询，对中文地区返回中文名称，其他地区返回本地化名称。

```json
{
  "status": "success",
  "country": "中国",
  "regionName": "广东",
  "city": "深圳",
  "isp": "中国电信",
  "query": "xxx.xxx.xxx.xxx"
}
```

海外 IP 示例:

```json
{
  "status": "success",
  "country": "United States",
  "regionName": "California",
  "city": "Los Angeles",
  "isp": "Cloudflare Inc.",
  "query": "104.16.0.0"
}
```

**备用**: `https://ipwho.is/{ip}`

全球覆盖，自带多语言支持，适合 ip-api.com 不可用时切换。

```json
{
  "success": true,
  "ip": "xxx.xxx.xxx.xxx",
  "country": "China",
  "region": "Guangdong",
  "city": "Shenzhen",
  "connection": { "isp": "China Telecom" }
}
```

### 5.3 请求策略

- 超时: 5 秒
- User-Agent: `VpnMonitor/1.0`
- 查询间隔: 默认 30 秒 (可配置 10s ~ 300s)
- IP 未变化时跳过归属地查询
- 连续失败 3 次后指数退避 (30s → 60s → 120s → 300s 上限)
- 成功后恢复正常间隔

---

## 6. GUI 详细设计

### 6.1 悬浮窗口样式

```
┌────────────────────────────────────────────────────────────┐
│ ●  xxx.xxx.xxx.xxx  │  中国 · 广东 · 深圳  │  中国电信     │
└────────────────────────────────────────────────────────────┘
  ↑        ↑                    ↑                  ↑
 状态灯   IP 地址             归属地             ISP
```

**窗口属性**:
- 位置: 屏幕顶部居中，任务栏上方
- 尺寸: 约 450×32 像素 (自适应内容)
- 样式: `WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW`
  - `TOPMOST`: 始终在最前
  - `LAYERED`: 支持半透明
  - `TOOLWINDOW`: 不在任务栏显示，不出现在 Alt+Tab 列表
- 透明度: 85% (可配置)
- 背景色: `#1E1E2E` (深色主题，与 Win11 暗色模式协调)
- 字体: `Segoe UI` / 系统默认
- 鼠标穿透: 可选 (`WS_EX_TRANSPARENT`)，通过快捷键切换

### 6.2 快捷键

| 快捷键 | 功能 | 说明 |
|--------|------|------|
| `Ctrl+Alt+H` | 切换悬浮窗显示/隐藏 | 隐藏后悬浮条不可见，后台继续检测 |
| `Ctrl+Alt+I` | 打开 IP 查询工具窗口 | 弹出独立窗口，输入任意 IP 查询归属地 |
| `Ctrl+Alt+Q` | 退出程序 | 唯一的退出方式（无托盘无关闭按钮） |

快捷键通过 Win32 `RegisterHotKey` 注册，全局生效，即使应用窗口不在焦点也能响应。

### 6.3 IP 查询工具窗口

由 `Ctrl+Alt+I` 触发弹出的独立窗口，用于查询任意 IP 的归属地信息。

```
┌──────────────────────────────────────┐
│  IP 地址查询                      ✕  │
├──────────────────────────────────────┤
│                                      │
│  IP 地址: [  104.16.0.0        ] [查] │
│                                      │
│  ┌────────────────────────────────┐  │
│  │  IP:     104.16.0.0           │  │
│  │  国家:   United States        │  │
│  │  地区：  California           │  │
│  │  城市:   Los Angeles          │  │
│  │  ISP:    Cloudflare Inc.      │  │
│  │  经纬度: 34.05, -118.24       │  │
│  └────────────────────────────────┘  │
│                                      │
│         [复制结果]   [关闭]           │
│                                      │
└──────────────────────────────────────┘
```

**窗口特性**:
- 标准对话框样式 (`WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU`)
- 尺寸: 400×350
- 居中显示，始终在悬浮窗之上
- 输入框支持 IPv4 / IPv6 / 域名
- Enter 键触发查询，Esc 键关闭窗口
- 查询完成后可一键复制结果到剪贴板

### 6.4 状态显示

| 状态 | 指示灯颜色 | 文本 |
|------|-----------|------|
| 正常 | 🟢 绿色 | `IP · 归属地 · ISP` |
| 检测中 | 🟡 黄色闪烁 | `正在检测...` |
| 网络不可达 | 🔴 红色 | `网络不可达` |
| API 限流 | 🟠 橙色 | `查询受限，稍后重试` |

---

## 7. 配置文件

路径: `%APPDATA%\Vpn_Monitor\config.toml`

```toml
[general]
# IP 检测间隔（秒）
check_interval = 30
# 开机自启
auto_start = true
# 鼠标穿透模式
click_through = false

[display]
# 窗口透明度 (0.0 ~ 1.0)
opacity = 0.85
# 窗口位置: "top-center" | "top-left" | "top-right"
position = "top-center"
# 显示内容字段
show_isp = true

[hotkey]
# 切换悬浮窗显示/隐藏 (Ctrl+Alt+H)
toggle_visibility = "ctrl+alt+h"
# 打开 IP 查询工具窗口 (Ctrl+Alt+I)
open_lookup = "ctrl+alt+i"
# 退出程序 (Ctrl+Alt+Q)
quit = "ctrl+alt+q"

[network]
# 请求超时（秒）
timeout = 5
# 最大重试次数
max_retries = 3
# 代理设置 (留空使用系统代理)
# proxy = "socks5://127.0.0.1:1080"
```

---

## 8. 数据结构

```rust
/// IP 更新消息（网络线程 → GUI 线程）
struct IpUpdate {
    ip: Option<String>,
    geo: Option<GeoInfo>,
    status: CheckStatus,
    checked_at: chrono::DateTime<chrono::Local>,
}

/// 归属地信息（全球支持）
struct GeoInfo {
    country: String,     // "中国" / "United States"
    region: String,      // "广东" / "California"
    city: String,        // "深圳" / "Los Angeles"
    isp: String,         // "中国电信" / "Cloudflare Inc."
}

/// 检测状态
enum CheckStatus {
    Success,
    NetworkError,
    ApiLimited,
    Checking,
}

/// 应用配置
struct AppConfig {
    check_interval: u64,
    auto_start: bool,
    click_through: bool,
    opacity: f32,
    position: WindowPosition,
    show_isp: bool,
    hotkey_toggle: String,    // "ctrl+alt+h"
    hotkey_lookup: String,    // "ctrl+alt+i"
    hotkey_quit: String,      // "ctrl+alt+q"
    timeout: u64,
    max_retries: u32,
    proxy: Option<String>,
}
```

---

## 9. 错误处理策略

| 场景 | 处理方式 |
|------|---------|
| 单个 IP API 超时 | 切换到备用 API |
| 所有 IP API 失败 | 显示"网络不可达"，指数退避重试 |
| 归属地 API 失败 | 只显示 IP，归属地显示"--" |
| 网络断开 → 恢复 | 检测到网络恢复后立即触发一次检测 |
| API 限流 (429) | 显示"查询受限"，延长检测间隔 |
| 快捷键冲突 | 注册失败时日志警告，使用备用快捷键组合 |
| 查询窗口输入无效 IP | 输入框下方提示"请输入有效的 IPv4/IPv6 地址" |
| 配置文件损坏 | 使用默认配置，日志记录警告 |

---

## 10. 开机自启 & 退出方式

### 开机自启

通过写入注册表实现:

```
HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run
  Vpn_Monitor = "C:\Program Files\Vpn_Monitor\vpn-monitor.exe"
```

### 退出方式

由于没有系统托盘图标和窗口关闭按钮，程序仅支持以下退出方式:

| 方式 | 操作 |
|------|------|
| 快捷键退出 | `Ctrl+Alt+Q` |
| 任务管理器 | 在"详细信息"页中结束 `vpn-monitor.exe` 进程 |
| 命令行 | `taskkill /IM vpn-monitor.exe` |

---

## 11. 项目依赖 (Cargo.toml)

```toml
[package]
name = "vpn-monitor"
version = "0.1.0"
edition = "2024"

[dependencies]
# Windows API
windows = { version = "0.59", features = [
    "Win32_Foundation",
    "Win32_UI_WindowsAndMessaging",
    "Win32_Graphics_Gdi",
    "Win32_System_LibraryLoader",
    "Win32_System_Threading",
] }

# 异步运行时
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync"] }

# HTTP 客户端
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }

# 序列化
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"

# 其他
chrono = "0.4"           # 时间处理
tracing = "0.1"          # 日志
tracing-subscriber = "0.3"
dirs = "5"               # 获取 %APPDATA% 路径
clap = { version = "4", features = ["derive"] }  # 命令行参数

[profile.release]
opt-level = "s"          # 优化体积
lto = true
strip = true
```

---

## 12. 编译 & 发布

```bash
# 开发
cargo build

# 发布编译
cargo build --release

# 目标文件位置
# target/release/vpn-monitor.exe  (~2MB，静态链接，无运行时依赖)
```

发布为单个 `.exe`，无需安装。用户放入任意目录，双击或加入开机自启即可。

---

## 13. 未来扩展（不在首版范围）

- [ ] VPN 连接/断开时的系统通知弹窗
- [ ] IP 变化历史记录 (SQLite)
- [ ] 多显示器支持 (指定显示在哪块屏幕)
- [ ] 自定义主题颜色
- [ ] 支持 SOCKS5/HTTP 代理检测
- [ ] 剪贴板 IP 自动识别并查询归属地
- [ ] 自定义快捷键绑定 UI
