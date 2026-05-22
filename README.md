# Vpn_Monitor

Windows 11 平台上的 IP 状态悬浮窗应用。自动检测当前公网 IP 及其归属地，在屏幕顶部以半透明悬浮条形式显示，同时展示 CPU、内存、实时网速等系统信息。纯后台运行，无系统托盘图标，通过全局快捷键控制。

## 功能特性

- 屏幕顶部半透明悬浮条，双行布局实时显示
- 第一行：Claude 模型标签 + IP 地址 + 城市 + 延迟 + 代理状态（仅未设置代理时显示）
- 第二行：上传/下载网速 + CPU 使用率 + 内存使用率
- 自动后台轮询，IP 变化时即时更新，IP 不变时复用归属地缓存
- 多源备份 IP 检测（ipify / ip.sb / ifconfig.me）
- 归属地查询双源备份（ip-api.com / ipwho.is）
- **网络异常显示原因**：超时 / DNS 失败 / 无连接 / TLS 错误 / HTTP 状态码，失败时仍保留上次已知 IP 和城市（暗色）
- **归属地失败显示原因**：IP 抓到了但归属地查询失败时，显示 `归属地? (限流/私有段/超时/...)` 而不是简单的 `--`
- 连续失败自动指数退避重试
- 多层代理检测：注册表 ProxyEnable / PAC URL > 已知代理进程名 > 代理专用端口
- **cc-switch 集成**：识别 `~/.cc-switch/settings.json` 当前 provider，配合 `~/.claude/settings.json` 的 `env.ANTHROPIC_MODEL` 显示模型/provider 名
- 快捷键打开 IP 查询工具窗口，可查任意 IP（含 ISP 信息）
- 窗口宽度自适应内容，每行内容水平居中；长文字自动截断
- **多显示器 / 高 DPI**：根据所在显示器工作区居中，响应 `WM_DPICHANGED`
- **周期性强制置顶**：3 秒重新断言 `HWND_TOPMOST`，对抗全屏程序/UAC 抢占
- **单实例守卫**：命名 mutex + FindWindow 双重检查，二次启动自动激活已有实例并退出
- 可选日志记录，支持自动清理过大日志

## 快捷键

| 快捷键 | 功能 |
|--------|------|
| `Ctrl+Alt+H` | 切换悬浮窗显示/隐藏 |
| `Ctrl+Alt+I` | 打开 IP 查询工具窗口 |
| `Ctrl+Alt+Shift+K` | 退出程序 |

## 编译

需要安装 [Rust](https://rustup.rs/) 工具链（MSVC target）。

```bash
# 调试编译
cargo build

# 发布编译（优化体积，约 2MB 单文件）
cargo build --release
```

编译产物位于 `target/release/vpn-monitor.exe`。

## 使用方法

1. 双击 `vpn-monitor.exe` 运行，屏幕顶部即出现悬浮条
2. 程序立即开始检测当前公网 IP 并查询归属地
3. 使用快捷键进行交互

### IP 查询工具窗口

按 `Ctrl+Alt+I` 打开，可输入任意 IP 地址查询归属地和 ISP 信息，支持一键复制结果。

## 配置文件

路径：与 `vpn-monitor.exe` 同目录的 `config.toml`（不存在则使用默认值）

```toml
check_interval = 10         # IP 检测间隔（秒），延迟也随此周期测量
auto_start = true           # 开机自启（预留）
click_through = false       # 鼠标穿透
opacity = 0.85              # 窗口透明度 (0.0 ~ 1.0)
position = "top-center"     # 窗口位置（预留）
show_isp = true             # 是否显示 ISP（查询窗口生效）

hotkey_toggle = "ctrl+alt+h"         # 切换显隐
hotkey_lookup = "ctrl+alt+i"         # 打开查询窗口
hotkey_quit = "ctrl+alt+shift+k"     # 退出程序

timeout = 5                 # 请求超时（秒）
max_retries = 3             # 最大重试次数
enable_log = false          # 是否启用日志记录
# proxy = "socks5://127.0.0.1:1080"  # 可选代理

monitor_interval = 2        # 系统监控刷新间隔（秒）：CPU/内存/网速
proxy_check_interval = 30   # 代理检测间隔（秒）：注册表+PAC+进程+端口
model_refresh_interval = 5  # Claude 模型标签刷新间隔（秒），0=仅启动时读取一次
```

### Claude 模型标签

第一行最左侧的标签按以下优先级解析：

1. `~/.claude/settings.json` 的 `env.ANTHROPIC_MODEL`（cc-switch 切换到 Zhipu / mcodex 等第三方 provider 时会写到这里，显示具体模型如 `glm-5` / `gpt-5.5`）
2. `~/.cc-switch/settings.json` 的 `currentProviderClaude`（如 `claude-official` 显示为 `Claude Official`）
3. 兜底显示 `Claude`

`model_refresh_interval` 控制重新读取间隔，cc-switch 切换 provider 后会自动跟新。设为 `0` 仅启动时读取一次。

### 网络异常显示

网络不可达时浮窗会保留上次已知的 IP 和城市（以暗色显示），并在末尾标注具体原因：

| 标签            | 含义                          |
| --------------- | ----------------------------- |
| `网络异常 (超时)` | 请求超时                       |
| `网络异常 (DNS 失败)` | 域名解析失败                  |
| `网络异常 (无连接)` | 无法建立 TCP 连接              |
| `网络异常 (TLS 错误)` | TLS/证书握手失败              |
| `网络异常 (HTTP 5xx)` | 服务器返回错误状态码          |
| `网络异常 (响应无效)` | 返回内容不是合法 IP          |

### 归属地缺失原因

IP 抓取成功但归属地查询失败时，浮窗第一行的城市位置会显示：

| 标签                | 含义                                       |
| ------------------- | ------------------------------------------ |
| `归属地? (限流)`    | ip-api / ipwho.is 限流（429 或 API 报错）   |
| `归属地? (私有段)`  | IP 在私有/保留段，API 拒绝返回数据         |
| `归属地? (超时)`    | 两家归属地服务都请求超时                    |
| `归属地? (网络)`    | DNS / 连接 / TLS 等网络层错误               |
| `归属地? (无效)`    | API 认为该 IP 不合法                       |
| `归属地? (解析失败)` | API 返回内容无法反序列化                   |
| `归属地? (未知)`    | 其他未分类错误                              |

`--` 仍用于"未尝试查询"的初始态。

### 日志

设置 `enable_log = true` 开启日志记录。日志文件位于 `%APPDATA%\Vpn_Monitor\vpn-monitor.log`，超过 5MB 时启动自动清空。

查看日志（PowerShell）：

```powershell
Get-Content "$env:APPDATA\Vpn_Monitor\vpn-monitor.log" -Encoding utf8 -Wait -Tail 50
```

## 退出方式

由于没有托盘图标和窗口关闭按钮：

- **快捷键**：`Ctrl+Alt+Shift+K`
- **任务管理器**：在"详细信息"页中结束 `vpn-monitor.exe`
- **命令行**：`taskkill /IM vpn-monitor.exe`

## 技术栈

- **语言**：Rust
- **GUI**：Win32 API（`windows-rs` crate）
- **HTTP**：`reqwest`（rustls TLS）
- **异步**：`tokio`
- **渲染**：GDI 原生绘制（ClearType 字体 + DWM 圆角）
- **系统监控**：`sysinfo`
