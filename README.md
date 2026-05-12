# Vpn_Monitor

Windows 11 平台上的 IP 状态悬浮窗应用。自动检测当前公网 IP 及其归属地，在屏幕顶部以半透明悬浮条形式显示。纯后台运行，无系统托盘图标，通过全局快捷键控制。

## 功能特性

- 屏幕顶部半透明悬浮条，实时显示 IP、归属地、ISP
- 自动后台轮询，IP 变化时即时更新
- 全球 IP 归属地查询支持（中国/海外均可）
- 多源备份 IP 检测（ipify / ip.sb / ifconfig.me）
- 归属地查询双源备份（ip-api.com / ipwho.is）
- 网络中断自动检测，恢复后自动恢复
- 连续失败自动指数退避重试
- 快捷键打开 IP 查询工具窗口，可查任意 IP

## 快捷键

| 快捷键 | 功能 |
|--------|------|
| `Ctrl+Alt+H` | 切换悬浮窗显示/隐藏 |
| `Ctrl+Alt+I` | 打开 IP 查询工具窗口 |
| `Ctrl+Alt+Q` | 退出程序 |

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

按 `Ctrl+Alt+I` 打开，可输入任意 IP 地址查询归属地信息，支持一键复制结果。

## 配置文件

路径：`%APPDATA%\Vpn_Monitor\config.toml`（不存在则使用默认值）

```toml
[general]
check_interval = 30       # IP 检测间隔（秒）
auto_start = true         # 开机自启（预留）
click_through = false     # 鼠标穿透

[display]
opacity = 0.85            # 窗口透明度 (0.0 ~ 1.0)
position = "top-center"   # 窗口位置（预留）
show_isp = true           # 是否显示 ISP

[hotkey]
toggle_visibility = "ctrl+alt+h"   # 切换显隐
open_lookup = "ctrl+alt+i"         # 打开查询窗口
quit = "ctrl+alt+q"                # 退出程序

[network]
timeout = 5               # 请求超时（秒）
max_retries = 3           # 最大重试次数
# proxy = "socks5://127.0.0.1:1080"  # 可选代理
```

## 退出方式

由于没有托盘图标和窗口关闭按钮：

- **快捷键**：`Ctrl+Alt+Q`
- **任务管理器**：在"详细信息"页中结束 `vpn-monitor.exe`
- **命令行**：`taskkill /IM vpn-monitor.exe`

## 技术栈

- **语言**：Rust
- **GUI**：Win32 API（`windows-rs` crate）
- **HTTP**：`reqwest`（rustls TLS）
- **异步**：`tokio`
- **渲染**：GDI 原生绘制
