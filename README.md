# Vpn_Monitor

Windows 11 平台上的 IP 状态悬浮窗应用。自动检测当前公网 IP 及其归属地，在屏幕顶部以半透明悬浮条形式显示，同时展示 CPU、内存、实时网速等系统信息。纯后台运行，无系统托盘图标，通过全局快捷键控制。

## 功能特性

- 屏幕顶部半透明悬浮条，双行布局实时显示
- 第一行：IP 地址 + 归属地（国家 · 城市）+ 代理状态
- 第二行：上传/下载网速 + CPU 使用率 + 内存使用率
- 自动后台轮询，IP 变化时即时更新，IP 不变时复用归属地缓存
- 多源备份 IP 检测（ipify / ip.sb / ifconfig.me）
- 归属地查询双源备份（ip-api.com / ipwho.is）
- 网络中断自动检测，恢复后自动恢复
- 连续失败自动指数退避重试
- 快捷键打开 IP 查询工具窗口，可查任意 IP（含 ISP 信息）
- 长文字自动截断显示
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

路径：`%APPDATA%\Vpn_Monitor\config.toml`（不存在则使用默认值）

```toml
check_interval = 10         # IP 检测间隔（秒）
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
```

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
