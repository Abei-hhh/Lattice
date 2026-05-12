# CLAUDE.md - Vpn_Monitor 项目指南

## 项目概述

Windows 11 后台悬浮窗应用，Rust 编写。顶部显示当前公网 IP + 归属地 + ISP。纯后台运行，通过全局快捷键交互。

## 架构

```
main.rs          → 入口：加载配置、启动 tokio runtime、spawn 网络轮询、GUI 消息循环
config.rs        → AppConfig 结构体 + TOML 加载 + 快捷键解析
gui/
  window.rs      → 悬浮窗口创建 + 消息循环（PeekMessage 轮询 + shutdown channel）
  render.rs      → GDI 绘制：背景、状态灯、IP 文本、归属地文本
  hotkey.rs      → RegisterHotKey / UnregisterHotKey（三个全局热键）
  lookup_dialog.rs → IP 查询工具窗口（CreateWindowExA + WM_COMMAND）
network/
  ip_fetcher.rs  → 多源并发获取公网 IP（ipify / ip.sb / ifconfig.me）
  geo_lookup.rs  → IP 归属地查询（ip-api.com 主用 / ipwho.is 备用）
```

## 关键设计决策

- **windows 0.59 crate**：函数返回 `Result<>`，HWND 需要 `Option<>` 包装，COLORREF 需要构造器
- **HWND 非 Send**：跨线程传递 HWND 时用 `usize` 中转
- **PeekMessage 轮询**：主循环手动轮询而非 GetMessage 阻塞，以便同时处理 tokio channel
- **clipboard/layered**：使用 `extern "system"` 直接声明，绕过 crate feature 缺失

## 常用命令

```bash
cargo build                # 调试编译
cargo build --release      # 发布编译（~2MB 单文件 exe）
```

## API 端点


| 用途       | URL                                      | 响应   |
| ---------- | ---------------------------------------- | ------ |
| 获取 IP    | `https://api.ipify.org`                  | 纯文本 |
| 归属地     | `http://ip-api.com/json/{ip}?lang=zh-CN` | JSON   |
| 备用归属地 | `https://ipwho.is/{ip}`                  | JSON   |

## 状态码

- 绿色 = 正常
- 黄色 = 检测中
- 红色 = 网络不可达
- 橙色 = API 限流

## 配置

路径：`%APPDATA%\Vpn_Monitor\config.toml`，详见 README.md。
