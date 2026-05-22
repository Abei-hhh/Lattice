# CLAUDE.md - Vpn_Monitor 项目指南

## 项目概述

Windows 11 后台悬浮窗应用，Rust 编写。顶部显示当前公网 IP + 归属地 + 系统监控（CPU/内存/网速）。纯后台运行，通过全局快捷键交互。

## 架构

```
main.rs          → 单实例守卫、按需初始化日志、tokio runtime、IP 轮询 task、Claude 标签刷新 task；监控独立 OS 线程
config.rs        → AppConfig + TOML 加载 + 快捷键解析
monitor.rs       → CPU/内存/网速 + 多层代理检测（同步循环，跑在 std::thread 上，避免阻塞 tokio）
gui/
  window.rs      → 悬浮窗主消息循环（MsgWaitForMultipleObjectsEx + mpsc 轮询）；Arc<WindowContext> 共享给子线程
  render.rs      → GDI 双行布局：Claude/IP/归属地/延迟/代理 + 网速/CPU/内存
  hotkey.rs      → RegisterHotKey / UnregisterHotKey（三个全局热键）
  lookup_dialog.rs → IP 查询窗口；worker 线程经 WM_APP_LOOKUP_RESULT PostMessage 回传结果
network/
  ip_fetcher.rs  → 多源并发抓 IP（ipify / ip.sb / ifconfig.me）+ 失败原因分类（FailReason 枚举）
  geo_lookup.rs  → 归属地：ip-api.com 主 / ipwho.is 备
```

## 关键设计决策

### Win32 / windows-rs
- **windows 0.59**：函数返回 `Result<>`，HWND 需 `Option<>` 包装，COLORREF 用 BGR 顺序构造器
- **Unicode API**：所有窗口/对话框用 W 变体，避免中文乱码
- **clipboard/layered**：用 `extern "system"` 直接声明，绕过 crate feature 缺失
- **DWM 圆角**：`DWMWCP_ROUND` + 匹配背景画刷 + ClearType，减少锯齿

### 窗口与生命周期
- **Arc<WindowContext>**：`Arc::into_raw` 存进 `GWLP_USERDATA`，`WM_NCDESTROY` 里 `Arc::from_raw` 回收；子线程（如 lookup 对话框）持 Arc 克隆，**消除主退出时的 UAF**
- **`lookup_dialog_open: AtomicBool`**：用 `compare_exchange` 防止重复打开查询窗口，且无数据竞争
- **跨线程 HWND**：用 `usize` 中转；跨线程更新 UI 一律走 `PostMessageW(WM_APP_*)` 加 IsWindow 校验，禁止子线程直接 `SetWindowTextW`
- **退出走 DestroyWindow**：`HOTKEY_QUIT` → `DestroyWindow` → `WM_DESTROY` → `PostQuitMessage` → `WM_NCDESTROY`（自动 unregister hotkey、KillTimer、回收 Arc）
- **Mutex 中毒恢复**：所有 state 锁通过 `lock_state` 辅助，poison 时 `into_inner()` 而非 panic

### 主消息循环
- **MsgWaitForMultipleObjectsEx**（替代 PeekMessage + 16ms sleep）：消息到来立即唤醒，16ms 超时回来轮询 mpsc channel
- **UiUpdate 统一通道**：IP 更新和系统监控共用一个 `mpsc`，UI 侧 try_recv 分发

### 置顶与多显示器
- **周期性 topmost**：3 秒 SetTimer，`WM_TIMER` 强制 `HWND_TOPMOST`，对抗全屏程序/UAC 抢占
- **窗口大小变化时也强制 topmost**：禁用 `SWP_NOZORDER`，每次 `SetWindowPos` 都用 `HWND_TOPMOST`
- **MonitorFromWindow + rcWork**：根据窗口所在显示器的工作区居中，非 `SM_CXSCREEN` 主屏
- **WM_DPICHANGED / WM_DISPLAYCHANGE**：跨屏切换不同 DPI 时按系统建议矩形重定位并触发重绘

### 单实例
- **三重防御**：
  1. 命名 mutex `Vpn_Monitor_SingleInstance_v1`，`CreateMutexW` 和 `GetLastError` 在**同一 unsafe 块**内捕获（防中间 syscall 清错误码）
  2. mutex 检查未拦下也用 `FindWindowA("VpnMonitorOverlay")` 兜底
  3. 找到旧窗口则 `ShowWindow` + 强制 topmost 后退出新进程
- 解决"快速双击导致多浮窗、热键失灵"的根因（第二实例注册热键失败，第一实例仍拥有，看似打不开/关不掉）

### Tokio vs OS 线程
- **监控走独立 OS 线程**：端口扫描（10 个 TCP connect_timeout）+ `sys.refresh_processes` 是同步阻塞，跑在 tokio 上会卡住 IP 轮询；改为 `std::thread::spawn(monitor_loop_sync)`
- **IP / Geo / Claude 刷新**保留在 tokio（reqwest 是 async）

### 渲染与状态
- **窗口自适应宽度**：每次 update 重测 row1/row2 文本宽，独立水平居中
- **延迟显示**：抓 IP 时记录耗时；<200ms 青色 / ≥200ms 橙色
- **网络失败保留上下文**：`CheckStatus::NetworkError` 时仍显示**上次已知**的 IP + 城市（暗色），后接 `网络异常 (原因)`；只有状态点变红
- **代理检测分层**：注册表 ProxyEnable / PAC URL > 已知代理进程名 > 代理专用端口（已剔除 8080/9090/2080 等易冲突的开发端口）
- **精简归属地**：第一行只显示城市；ISP 仅在查询窗口显示

### Claude 标签解析（cc-switch 集成）
优先级（高 → 低）：
1. `~/.claude/settings.json` `env.ANTHROPIC_MODEL`（cc-switch 切第三方 provider 时会写到这里）
2. `~/.cc-switch/settings.json` `currentProviderClaude` → 友好名（`claude-official` → "Claude Official"；UUID 形态 → "Claude"）
3. 兜底 "Claude"

不读 cc-switch 的 SQLite（避免 800KB 依赖），因为 model 字段已经被 cc-switch 镜像到 ~/.claude/settings.json env 里。

### 网络失败原因分类
- **IP 抓取**：`ip_fetcher::FailReason` 枚举 — `Timeout / Connect / Dns / Tls / Http(u16) / Decode / Other`。基于 `reqwest::Error` 的 `is_timeout/is_connect/is_decode/status` + 错误字符串关键词分类；多源失败时按"诊断价值"聚合（DNS > Connect > TLS > Timeout > HTTP > Decode > Other）
- **归属地查询**：`geo_lookup::GeoFailReason` 枚举 — `Timeout / Network / Private / Invalid / Decode / Other`。除了网络层错误，还识别 ip-api / ipwho.is 返回的 `private range` / `reserved range` / `invalid query` message
- **IpUpdate 双错误字段**：`error_reason`（网络异常文本，红色）与 `geo_error_reason`（归属地缺失原因，暗色），UI 同时展示两类问题
- 归属地失败时浮窗显示形如 `归属地? (限流)` / `归属地? (私有段)` / `归属地? (超时)`，与"网络异常 (...)"区分开

### 启动期失败处理
- 不再用 `expect(...)` 触发隐式 panic（GUI 子系统下 panic 看起来就是"启动了一下没了"）
- HTTP client 构建、tokio runtime 创建、日志文件创建失败一律 `MessageBoxW` 提示后正常退出
- 日志文件创建失败时仅禁用文件日志，不阻断启动

## 常用命令

```bash
cargo build                # 调试编译
cargo build --release      # 发布编译（~2MB 单文件 exe）
```

发布编译前需确保没有运行中的 `vpn-monitor.exe` 锁定输出。

## API 端点

| 用途       | URL                                      | 响应   |
| ---------- | ---------------------------------------- | ------ |
| 获取 IP    | `https://api.ipify.org` / `api.ip.sb/ip` / `ifconfig.me/ip` | 纯文本 |
| 归属地     | `http://ip-api.com/json/{ip}?lang=zh-CN` | JSON   |
| 备用归属地 | `https://ipwho.is/{ip}`                  | JSON   |

## 状态灯

- 绿色（`#4CAF50`）= 正常
- 蓝色（`#2196F3`）= 检测中
- 红色（`#F44336`）= 网络不可达；显示形如 `网络异常 (超时)` / `(DNS 失败)` / `(HTTP 503)`
- 橙色（`#FF6F00`）= API 限流

## 配置

路径：与 `vpn-monitor.exe` 同目录的 `config.toml`。默认值：
- `model_refresh_interval = 5`（默认 5 秒刷新 Claude 标签，捕获 cc-switch 切换）
- `proxy_check_interval = 30`
- `monitor_interval = 2`
- `timeout = 5`、`max_retries = 3`、`check_interval = 10`

详见 README.md。
