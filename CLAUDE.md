# CLAUDE.md - Vpn_Monitor 项目指南

## 项目概述

Windows 11 后台悬浮窗应用，Rust 编写。顶部显示当前公网 IP + 归属地 + 系统监控（CPU/内存/网速）。纯后台运行，通过全局快捷键交互。

## 架构

```
main.rs          → 入口：加载配置、按需初始化日志、启动 tokio runtime、spawn IP轮询/系统监控任务、GUI 消息循环
config.rs        → AppConfig 结构体 + TOML 加载 + 快捷键解析
monitor.rs       → 系统监控：CPU使用率、内存使用率、网络速度（sysinfo crate）
gui/
  window.rs      → 悬浮窗口创建 + 消息循环（PeekMessage 轮询 + UiUpdate 统一通道）
  render.rs      → GDI 绘制：双行布局 - IP/归属地/代理状态 + 网速/CPU/内存
  hotkey.rs      → RegisterHotKey / UnregisterHotKey（三个全局热键）
  lookup_dialog.rs → IP 查询工具窗口（CreateWindowExW Unicode 版 + WM_COMMAND）
network/
  ip_fetcher.rs  → 多源并发获取公网 IP（ipify / ip.sb / ifconfig.me）
  geo_lookup.rs  → IP 归属地查询（ip-api.com 主用 / ipwho.is 备用）
```

## 关键设计决策

- **windows 0.59 crate**：函数返回 `Result<>`，HWND 需要 `Option<>` 包装，COLORREF 需要构造器
- **HWND 非 Send**：跨线程传递 HWND 时用 `usize` 中转
- **PeekMessage 轮询**：主循环手动轮询而非 GetMessage 阻塞，以便同时处理 tokio channel
- **clipboard/layered**：使用 `extern "system"` 直接声明，绕过 crate feature 缺失
- **Unicode API**：所有窗口和对话框使用 W（宽字符）变体，避免中文乱码
- **归属地缓存**：轮询循环中维护 `last_geo`，IP 未变时复用缓存
- **日志系统**：通过 `enable_log` 配置开关，UTF-8 BOM，超过 5MB 自动清空
- **UiUpdate 统一通道**：IP 更新和系统监控共用一个 `mpsc` 通道，UI 侧统一分发
- **DWM 圆角**：使用 `DWMWCP_ROUND` + 匹配背景画笔 + ClearType 字体，减少边缘锯齿
- **COLORREF 颜色**：注意 BGR 顺序（0x00BBGGRR），状态灯颜色采用 Material Design 色板

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

## 状态灯

- 绿色（`#4CAF50`）= 正常
- 蓝色（`#2196F3`）= 检测中
- 红色（`#F44336`）= 网络不可达
- 橙色（`#FF6F00`）= API 限流

## 配置

路径：`%APPDATA%\Vpn_Monitor\config.toml`，详见 README.md。
