# CLAUDE.md - Vpn_Monitor 项目指南

## 项目概述

Windows 11 后台悬浮窗应用，Rust 编写。顶部显示当前公网 IP + 归属地 + 系统监控（CPU/内存/网速）。纯后台运行，通过全局快捷键交互。

## 架构

**Workspace 结构**（Phase 0 已完成）：

```
Vpn_Monitor/
├── Cargo.toml             ← [workspace] + binary [package]，公共依赖在 [workspace.dependencies]
├── crates/
│   └── core/              ← 平台无关核心（Linux/macOS 上 cargo build -p vpn-monitor-core 也能编）
│       ├── Cargo.toml     ← vpn-monitor-core lib
│       └── src/
│           ├── lib.rs
│           ├── config.rs       ← AppConfig + TOML 加载 + 快捷键解析
│           ├── cc_switch.rs    ← claude / codex / gemini / opencode / hermes / openclaw 多源读取
│           ├── runtime.rs      ← RuntimeFlags：AtomicBool + RwLock<String> 跨线程共享
│           ├── usage.rs        ← cc-switch SQLite 5h/周用量聚合 + 明细查询（rusqlite bundled）
│           ├── proxy_rpc.rs    ← Clash / Mihomo / sing-box clash-api 客户端（自动探测 9090/9001/6170）
│           └── network/
│               ├── mod.rs
│               ├── ip_fetcher.rs   ← 多源并发抓 IP + mask_ip/mask_geo/mask_proxy_url 脱敏
│               ├── geo_lookup.rs   ← 双 provider 跨源校验
│               ├── geo_cache.rs    ← /24 网段 LRU 磁盘缓存
│               └── leak_check.rs   ← DNS / IPv6 泄漏检测（v6 IP + Cloudflare /cdn-cgi/trace）
├── src/                    ← Windows 桌面 binary（GUI 壳 + 平台特定监控）
│   ├── main.rs                 ← 单实例守卫、tokio runtime、6 个后台 task；
│   │                             `pub use vpn_monitor_core::{...}` 让 gui/ 子模块继续用
│   │                             `crate::config::*` 等路径无需大改
│   ├── monitor.rs              ← Win32 注册表代理检测、端口并发扫描、GetLastInputInfo 空闲探测
│   ├── tcp_table.rs            ← GetExtendedTcpTable 拿活跃 TCP 远端 IP → GeoCache 反查国家分布
│   └── gui/                    ← 全 Win32 UI（详见下文）
├── assets/app.svg          ← 应用图标源文件
└── build.rs                ← 编译期 SVG → 多尺寸 .ico → embed-resource 链接进 exe 资源段
```

**src/gui/ 模块职责**：

| 文件 | 职责 |
|---|---|
| `window.rs` | 悬浮窗主消息循环（MsgWaitForMultipleObjectsEx + mpsc 轮询）；Arc<WindowContext>；WM_NCHITTEST 拖动；WM_EXITSIZEMOVE 位置持久化；WM_POWERBROADCAST 唤醒；WM_APP_TRAY 路由；全菜单 WM_COMMAND |
| `render.rs` | GDI 渲染；支持 simple/detailed 两种形态；row2 system/usage 双模式；detailed 模式右侧 sparkline + 国家分布堆叠条 |
| `theme.rs` | Light/Dark 色板 + 系统主题探测（HKCU AppsUseLightTheme）+ DwmSetWindowAttribute dark caption 助手 |
| `md3.rs` | MD3 风格 owner-draw 按钮（RoundRect + 主题色 + 焦点描边） |
| `hotkey.rs` | RegisterHotKey / UnregisterHotKey（三个全局热键） |
| `overlay_state.rs` | 浮窗位置 + 锁定状态持久化到 overlay_state.json（独立于 config.toml） |
| `tray.rs` | Shell_NotifyIconW 托盘图标 + 两层右键菜单 + ShellExecuteW 辅助 |
| `lookup_dialog.rs` | IP 查询窗口；支持 initial_ip 预填（历史窗口"双击重查"用）；先查 GeoCache 命中再打 API |
| `history_dialog.rs` | IP 历史时间线 ListView 窗口：搜索 / 双击重查 / 右键复制·删除 / CSV 导出 |
| `settings_dialog.rs` | 高级设置 5-Tab 对话框：全字段编辑，toml_edit 保留注释，分立即生效/重启生效 |
| `usage_dialog.rs` | AI 用量明细窗口：4 时段 radio + ListView（工具/Provider/模型/请求数/Tok/费用/延迟） |

**workspace 路径备忘**：

- binary 子 crate（`src/`）内部仍可继续用 `crate::config::*` / `crate::network::*` 等老路径 ——
  `main.rs` 顶部一行 `pub use vpn_monitor_core::{cc_switch, config, network, runtime};` 做了 alias。
- 若新写代码，推荐直接 `use vpn_monitor_core::config::AppConfig;` 表达更清楚的跨 crate 依赖关系。

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

### 即时重查通道（Notify）
- 单个 `Arc<tokio::sync::Notify>` 由 IP 轮询任务 await，谁 `notify_one()` 都会立刻打断 sleep 进入下一轮
- 两个调用方：
  1. **window_proc** WM_POWERBROADCAST（PBT_APMRESUMEAUTOMATIC / RESUMESUSPEND） — 笔记本合盖恢复后立即抓 IP
  2. **monitor_loop_sync** 检测到代理状态翻转时 — 切节点立即反映新 IP
- 收到 notify 后 `last_ip = None` + `consecutive_failures = 0` + `current_interval` 复位 —— 避免唤醒后立刻显示 NetworkError 红点

### Geo 缓存 (`network/geo_cache.rs`)
- **/24 网段归并**：v4 `a.b.c.0/24`、v6 前 48 位作 key —— 同 ISP 节点池命中率从 ~30% 跃升到 ~80%
- **LRU + 上限**：`VecDeque` 维护访问顺序，超 `geo_cache_max_entries`（默认 1000）淘汰最老
- **原子写盘**：每次 insert 全量重写 JSON（量级 KB，便宜），`tmp + rename` 防中途崩溃留半写
- **路径**：`%APPDATA%\Vpn_Monitor\geo_cache.json`
- **公开 API**：`get(ip)` / `insert(ip, geo)` / `remove(key)` / `history()` — 后两个分别给设置·历史窗口用
- **旧格式平滑迁移**：发现 JSON 是裸 HashMap（早期版本）自动转成新 `DiskFormat { entries, lru }`

### 跨源国别校验（HTTPS-优先）
- `lookup_geo(.., cross_check: bool)` 两个分支：
  - `true`（默认 + IP 轮询）：`tokio::join!` 等两个 provider，HTTPS（ipwho.is）结果优先；两边国家不一致时 `tracing::warn!` 加 `warning` 字段回传 UI
  - `false`（lookup 对话框）：竞速取最快 Ok 的那个，纯延迟最优
- `GeoLookupOutcome::Ok { geo, warning: Option<String> }` —— warning 透传到 `IpUpdate.geo_warning`，渲染层在第一行尾部画橙色 `⚠ 跨源不一致: HTTPS=US/HTTP=CN`
- HTTPS 失败、回退 HTTP 时也回传 `warning = Some("仅 HTTP 源（HTTPS 失败）")` 提示用户：当前结果可被中间人篡改

### 日志脱敏
- `ip_fetcher` 三个工具：
  - `mask_ip(ip)` → v4 `1.2.x.x` / v6 `xxxx:x:x:x:x:x:x:x`
  - `mask_geo(s)` → FNV-1a 64 hash → `geo:xxxxxxxx`
  - `mask_proxy_url(url)` → `scheme://***@host` 去掉凭证
- 全局 `AtomicBool`（之前是 `OnceLock`，改成 AtomicBool 支持运行时切换）
- 浮窗 / 对话框始终显示真值；只在 `tracing::*!` 路径走脱敏

### 监控线程的稳定性 / 性能
- **sysinfo 增量刷新**：`refresh_processes_specifics(All, true, ProcessRefreshKind::nothing())` 只刷进程名，单次成本降 ~5–10×
- **端口扫描并发**：N 个线程各 connect_timeout，任一 Ok 写 `AtomicBool`，其他线程下次 load 时短路；最差 ≈ 一次超时
- **空闲降频**：`GetLastInputInfo` 探测用户空闲秒数 ≥ `idle_threshold_seconds` 时所有间隔 ×`idle_multiplier`
- **panic 兜底**：监控线程外层 `catch_unwind(AssertUnwindSafe(..))`，崩了最多重启 10 次，每次 2s 退避

### 浮窗拖动 / 锁定 / 位置持久化
- **拖动**：WM_NCHITTEST 默认 HTCLIENT，未锁定时提升为 HTCAPTION，OS 接管拖动
- **`auto_center: AtomicBool`**：默认 true 时宽度变化重新居中；拖动 / 加载持久化位置后翻转为 false，宽度变化用 SWP_NOMOVE 保留位置
- **持久化**：`overlay_state.json` 独立文件存 `{ x, y, locked }`，每次拖动结束（WM_EXITSIZEMOVE）和正常退出（WM_DESTROY）都写盘
- **不写 config.toml**：拖动频繁，避免覆盖主配置丢注释

### 托盘图标 + 多层右键菜单
- Shell_NotifyIconW 注册嵌入的应用图标（资源 ID 1）
- 自定义 `WM_APP_TRAY = WM_APP+2`：lparam 低字为鼠标事件，左/右键 up 都召唤菜单
- 菜单结构：
  - **二级子菜单**（CreatePopupMenu + AppendMenuW(MF_POPUP)）：
    - `显示设置 ▸` —— 显示浮窗 / 锁定位置 / 鼠标穿透
    - `浮窗形态 ▸` —— 简易 / 完整（含流量曲线）
    - `第二行模式 ▸` —— 系统资源 / AI 用量
    - `隐私 & 缓存 ▸` —— 日志掩码 IP / 日志掩码归属地 / 启用归属地缓存 / HTTPS 跨源校验
    - `文件 ▸` —— 打开 config.toml / 打开日志目录
  - **顶层动作**（频繁用，直达）：IP 查询 / 历史时间线 / 用量明细 / 高级设置 / 退出
- 子菜单 HMENU 不需手动 DestroyMenu，DestroyMenu 父菜单会递归销毁
- **立即生效**字段全部走 RuntimeFlags AtomicBool 或 ip_fetcher 静态 atomic，菜单点完下一次 tick 已生效

### 设置对话框（settings_dialog.rs）
- 5-Tab：`常规 / 网络 / 隐私 & 安全 / 热键 / 高级`
- 所有控件一次性创建，按 tab 索引 ShowWindow(SW_SHOW/SW_HIDE) 切换，状态零丢失
- **保存路径**：`toml_edit` 加载 config.toml → 逐 key `doc["k"] = value(v)` → tmp+rename 原子写 → **注释 / 顺序 / 缩进全部保留**
- **立即生效字段**：opacity / click_through / mask_ip / mask_geo / geo_cache_enabled / geo_cross_check
- **需重启字段**：所有 `*_interval` / timeout / max_retries / 热键 / 缓存大小 / proxy / enable_log —— 保存时弹 MessageBox 提示

### 应用图标（build.rs + resvg → ICO + embed-resource）
- `assets/app.svg` 主题：盾形 + 3 节点 + 主色 `#4CAF50`（与 ACCENT_GREEN 一致）
- `build.rs` 流水线：SVG → usvg/resvg 光栅化到 256/48/32/16 RGBA → PNG 编码 → 手写 ICO 文件头 + 目录条目 → `embed_resource::compile` 链接到 exe
- PNG 编码自带（不引 image crate）：store-only deflate + adler32 + crc32，零外部依赖、~256KB ico
- 资源 ID 一律用 `1`，托盘 / 主窗口 / 三个对话框都 `LoadIconW(hinstance, PCWSTR(1 as *const _))` 加载

### 主题系统（默认 / 白天 / 黑夜）
- `theme.rs::Theme` 结构含 14 个角色色（bg / surface / fg_primary/secondary/dim / accent_* / separator / latency）
- `LIGHT` / `DARK` 两个 const 预设；`"system"` 模式查 HKCU `AppsUseLightTheme` REG_DWORD（1=light）
- `RuntimeFlags.theme_mode: Arc<RwLock<String>>` 跨线程共享当前 mode
- `SharedState.theme: Theme` 装解析出的色板，`render.rs` 全部颜色取自 `state.theme.*`
- **运行时切换**：设置对话框点应用 → 改 RuntimeFlags.theme_mode → `notify_theme_changed(hwnd)` PostMessage → 主线程 WM_APP_THEME_CHANGED 重读 mode + resolve + InvalidateRect
- **跟随系统**：主窗口监听 WM_SETTINGCHANGE，mode = "system" 时重新探测并重画
- **对话框暗色标题栏**：三个对话框创建后调 `theme::apply_dark_titlebar(hwnd, dark)`，底层是 `DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`（Win11 / Win10 1809+ 原生）
- **ListView 暗色**：历史窗口暗色模式调 `SetWindowTheme(list, w!("DarkMode_Explorer"), None)`

### Material Design 3 按钮
- `md3.rs::draw_button(dis, theme, is_primary)` 用 `RoundRect` 画 12px 圆角 + 主题色填充 + ClearType 居中文字
- 主操作（"确定"、"刷新"、"查询"）填充 `accent_green`，白字；次要操作填充 `surface`，主题前景文字
- 按下态把背景 darken 0.85，焦点态加 2px 描边
- 接入方式：button CreateWindow 加 `super::md3::BS_OWNERDRAW_STYLE`，对话框 dialog_proc 处理 WM_DRAWITEM 转发即可
- **未做（Wave 4 候选）**：hover 态需要每个 button subclass + TrackMouseEvent；托盘菜单替换为自绘 popup window；checkbox/edit 自绘

### cc-switch 多源（claude / codex / gemini / opencode / hermes / openclaw）
- `cc_switch::KNOWN_TOOLS` 白名单 6 项；`detect_available_sources()` 扫 `~/.cc-switch/settings.json` 所有 `currentProvider<Tool>` 字段提取真实安装的工具
- `read_label(source)` 三层 fallback：
  1. source == "claude" → 先看 `~/.claude/settings.json` env.ANTHROPIC_MODEL
  2. cc-switch settings.json `currentProvider<TitleCase>` → 套友好名映射（如 `claude-official` → "Claude Official"，UUID 形态 → 工具名）
  3. 工具名首字母大写兜底（"Gemini" / "Codex"）
- 设置对话框 "高级" tab 提供 6 个 radio（未配置的工具加 "(未配置)" 后缀但仍可选）
- 切换时 `RuntimeFlags.active_cc_switch_provider: Arc<RwLock<String>>` 立即更新 + `set_overlay_claude_label` PostMessage 浮窗强制重画

### 历史时间线窗口（history_dialog.rs）
- SysListView32 报表视图 + `LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES | LVS_EX_DOUBLEBUFFER`
- 列：时间 / IP / 国家 / 城市 / ISP / 网段
- **搜索**：顶部 Edit + EN_CHANGE 触发子串过滤（IP/国家/城市/ISP/网段 任一命中）
- **双击行**：NM_DBLCLK → 用该 IP 打开 lookup 对话框（`with_initial_ip` 预填 + 自动 PostMessage 触发查询）
- **右键菜单**（NM_RCLICK）：复制 IP / 复制完整行 / 从缓存删除（调 `GeoCache::remove`）
- **CSV 导出**：GetSaveFileNameW + UTF-8 BOM（Excel 才认中文）+ RFC 4180 转义
- 所有过滤 / 删除都操作内存里 `visible_entries`，原始 `all_entries` 留作刷新对照

### 浮窗双形态（simple / detailed）
- `overlay_form` 字段 + RuntimeFlags.overlay_form（Arc<RwLock<String>>）热切换
- ROW_HEIGHT 调整为 28（原 26，加大呼吸感）；新增 ROW3_HEIGHT = 64
- **simple**：双行布局，总高 56（2 × 28）
- **detailed**：**3 行**布局，总高 120（2 × 28 + 64）
  - **第三行（全宽）**：顶部 8px **国家分布堆叠条** + 紧邻的国家 legend（top-3 "● US 60%"）+ 主体全宽双折线 sparkline
  - 国家分布：来自 `tcp_table::summarize_by_country` [(country, count)]，颜色按字符串 FNV-1a hash → HSV
  - 流量曲线：60 个 (up_bps, down_bps) 采样点（每次 monitor tick push），双折线，up=fg_latency 青色 / down=accent_green，按窗口 max 自动归一化
  - 右下角小字标当前最高速率
- 托盘菜单 `IDM_FORM_SIMPLE` / `IDM_FORM_DETAILED` 切换 → RuntimeFlags + state 同步 + recalc_overlay_width（手动重测宽 + 重设高度 + SetWindowPos）

### Row 2 双模式（system / usage）
- `row2_mode` 字段 + RuntimeFlags.row2_mode 热切换
- **system**：↑/↓ + CPU + 内存（沿用 monitor 数据）
- **usage**：主用模型 + 5小时配额% + 倒计时 + 7天配额% + 倒计时
  - 显示格式：`{model} · 5小时:26% 41m · 7天:56% 1d1h`
  - **百分比基准 = 真实用户消息数 / 配置上限**（与 cc-switch UI 口径一致）
  - 真实用户消息从 `~/.claude/projects/**/*.jsonl` 解析：行含 `"type":"user"` 且 **不含** `tool_use_id`（后者是 Claude 工具调用结果回传，1 次用户消息可触发 5–10 次这种回传）
  - 之前用 `proxy_request_logs.request_count` 会把 1 条消息算 ~7 次（含工具循环），用 `total_cost_usd` 会因 API 列表价 ≫ 订阅价而严重偏高，都不对
  - 文件列表通过 cc-switch 自己维护的 `session_log_sync` 表取，只读 `last_modified` 落在窗口内的（性能优化）
  - 时间戳用手写 ISO8601 → unix 转换，不引 chrono 到 core
  - 配置：`usage_5h_limit_requests`（默认 50，Anthropic Pro 真实配额；Max 用户改 250）/ `usage_week_limit_requests`（默认 1000，Max 改 5000）
  - 倒计时 = 窗口内**最早用户消息** + 窗口长度 - now；格式：<60m → `Nm`、<24h → `NhMm`、≥24h → `NdMh`
  - 百分比着色：<60% 次要色 / 60-85% 橙 / >85% 红
  - 配额 = 0 时退化为只显示绝对消息数
- 托盘菜单 `IDM_ROW2_SYSTEM` / `IDM_ROW2_USAGE` 切换

### cc-switch SQLite 用量集成（core/usage.rs）
- 读 `~/.cc-switch/cc-switch.db` 的 **proxy_request_logs** 表（cc-switch proxy 模式下记录每次请求）
- `rusqlite = "0.32"` features = ["bundled"] —— 静态编译 sqlite3 源码，零系统依赖，增 ~700KB
- 只读打开 (`SQLITE_OPEN_READ_ONLY`)，对 cc-switch 写入零影响（WAL 模式）
- `read_usage_stats(app_type)` 返回 `UsageStats { window_5h, window_week }`，每个 `UsageWindow` 含 req_count / input_tokens / output_tokens / cache_read / total_cost_usd / top_model
- `list_usage_breakdown(since_secs)` 给用量明细窗口用：按 (app_type, provider_id, model) GROUP BY，cost 降序
- 后台 task 每 30s 刷新一次（`usage_refresh_interval`），写到 `OverlayState.usage`
- 时区策略：window_week 用 "now − 7×24h 滚动"近似（避免引 chrono 到 core），用户感知一致

### DNS / IPv6 泄漏检测（core/network/leak_check.rs）
- 三路并发探测：
  1. **v4 country**：复用主 IP 轮询拿到的国家
  2. **v6 country**：调 `api6.ipify.org`（强制 v6 路径）拿 v6 IP → geo_lookup 查国家；机器无 v6 → None（不算泄漏）
  3. **DNS country**：调 `https://1.1.1.1/cdn-cgi/trace`，解析 `loc=XX` 行（Cloudflare 看到的 DNS 解析者 ISO 国别）
- 短超时（3s），任一失败安静降级为 None
- `v4_country != v6_country`（两者非空）→ `v6_leak = true`
- `v4_country != dns_country`（两者非空）→ `dns_leak = true`
- UI 表现：浮窗第一行尾部红色 `[v6泄漏]` / `[DNS泄漏]` 徽章
- 后台 task 每 2 分钟刷新（leak 不变频繁，节省 HTTPS 开销）

### Clash / Mihomo / sing-box RPC 集成（core/proxy_rpc.rs）
- 自动探测 `127.0.0.1:9090`（Clash/Mihomo 默认）→ 9001（fork）→ 6170（sing-box clash-api）
- GET `/version` 鉴别工具（Mihomo 返回 `meta: true`、sing-box 返回 `version: "sing-box ..."`、Clash 默认）
- GET `/proxies` 找 `type == "Selector"` 的组（优先名字含 select/proxy/代理/节点/GLOBAL），取 `now` 字段 = 当前选中节点名
- 后台 task 每 5s 探测一次，写到 `OverlayState.proxy_rpc: Option<ProxyRpcSnapshot>`
- UI 表现：浮窗第一行尾部用 `→ {节点名}` 绿色标签替代原 `未设置代理` 文本（节点名通常自带 emoji 国旗）

### 流量分流可视化（src/tcp_table.rs）
- Win32 `GetExtendedTcpTable(TCP_TABLE_BASIC_CONNECTIONS, AF_INET)` 拿所有 IPv4 TCP 连接
- 只看 ESTABLISHED (state = 5) 的远端 IP，去重后按 GeoCache 命中聚合国家
- 跳过私有/CGNAT/loopback（10.0.0.0/8、172.16/12、192.168/16、100.64/10、127.0.0.0/8）
- 未命中 cache 的远端 IP 归到"未知"桶
- top_n=5 之外的国家合并到"其它"桶
- 独立 OS 线程每 10s 扫一次 → 写到 `OverlayState.traffic_by_country`
- UI 只在 detailed 形态显示（堆叠条）

### 用量明细窗口（usage_dialog.rs）
- 顶部 4 个 radio：最近 5h / 24h / 7d / 30d，切换即重新 SQL GROUP BY 查询
- ListView 列：工具 / Provider / 模型 / 请求数 / 输入 Tok（K/M 短形）/ 输出 Tok / 费用 USD / 平均延迟
- 按 cost 降序，所有 provider × model 组合全部列出
- 同 cc-switch SQLite 只读打开，对 cc-switch 写入零影响
- 跟随主题 + MD3 owner-draw 按钮（关闭按钮）

## 后台 task 一览（main.rs 启动后并发跑）

| Task | 间隔 | 用途 |
|---|---|---|
| IP 轮询 | check_interval (10s) | 公网 IP + 归属地 |
| 监控线程（OS 线程） | monitor_interval (2s) | CPU/内存/网速/代理检测 |
| 代理变化通知 | 事件驱动 | 监控检测到 proxy 翻转 → notify_one() 唤醒 IP 轮询 |
| 模型标签刷新 | model_refresh_interval (5s) | 读 cc-switch active provider 当前模型 |
| **用量统计刷新** | usage_refresh_interval (30s) | 读 cc-switch SQLite 5h+周用量 |
| **代理 RPC 探测** | 5s | Clash/sing-box 当前节点名 |
| **泄漏检测** | 120s | DNS / v6 泄漏 |
| **TCP 表扫描**（OS 线程） | 10s | 国家分布（detailed 模式专用） |

## 常用命令

```bash
cargo build                # 调试编译
cargo build --release      # 发布编译（~2.5MB 单文件 exe，含 toml_edit）
```

发布编译前需确保没有运行中的 `vpn-monitor.exe` 锁定输出。

**运行时无需重启的修改**：托盘菜单切换的所有开关 + 高级设置对话框中标"✅"的字段 + 拖动浮窗。其余字段编辑后会弹"需重启"提示。

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

路径：与 `vpn-monitor.exe` 同目录的 `config.toml`。**首次启动自动生成默认文件**。

通过托盘 → "高级设置..." 可视化编辑（保留注释）；也可直接编辑 toml，**部分字段需重启**生效。

### 完整字段表

| 字段 | 默认 | 含义 | 立即生效？ |
|---|---|---|---|
| `check_interval` | 10 | IP 检测间隔（秒）；延迟也随此周期测量 | 重启 |
| `auto_start` | true | 开机自启（预留，未实现） | — |
| `click_through` | false | 鼠标穿透 | ✅ |
| `opacity` | 0.85 | 浮窗不透明度 (0.0–1.0) | ✅ |
| `position` | "top-center" | 位置（预留；实际由 overlay_state.json 持久化） | — |
| `show_isp` | true | 是否在查询窗口显示 ISP | 重启 |
| `hotkey_toggle` | ctrl+alt+h | 显隐快捷键 | 重启 |
| `hotkey_lookup` | ctrl+alt+i | IP 查询快捷键 | 重启 |
| `hotkey_quit` | ctrl+alt+shift+k | 退出快捷键 | 重启 |
| `timeout` | 5 | HTTP 请求超时（秒） | 重启 |
| `max_retries` | 3 | 连续失败几次后浮窗显示红色 | 重启 |
| `proxy` | None | 出口代理 URL (可选) | 重启 |
| `enable_log` | false | 写日志到 `%APPDATA%\Vpn_Monitor\` | 重启 |
| `monitor_interval` | 2 | CPU/内存/网速 刷新间隔（秒） | 重启 |
| `proxy_check_interval` | 30 | 代理检测间隔（秒） | 重启 |
| `model_refresh_interval` | 5 | Claude 标签刷新（秒），0 关闭 | 重启 |
| `mask_ip_in_log` | true | 日志中 IP 掩码为 `1.2.x.x` | ✅ |
| `mask_geo_in_log` | true | 日志中归属地 hash 脱敏 | ✅ |
| `geo_cache_enabled` | true | 启用归属地磁盘缓存 | ✅ |
| `geo_cache_ttl_hours` | 168 | 缓存有效期，默认 7 天 | 重启 |
| `geo_cache_max_entries` | 1000 | LRU 上限，超过淘汰最老 | 重启 |
| `idle_threshold_seconds` | 900 | 用户空闲多少秒后降频；0 关 | 重启 |
| `idle_multiplier` | 5 | 空闲时所有轮询间隔的倍数 | 重启 |
| `geo_cross_check` | true | 跨源（HTTPS/HTTP）国别校验 | ✅ |
| `theme` | "system" | UI 主题：system / light / dark | ✅ |
| `active_cc_switch_provider` | "claude" | 浮窗左上 tag 显示哪个 cc-switch 工具的模型 | ✅ |
| `overlay_form` | "simple" | 浮窗形态：simple（双行）/ detailed（双行 + 流量曲线 + 国家分布） | ✅（托盘菜单） |
| `row2_mode` | "system" | 第二行：system（↑↓+CPU+内存）/ usage（主模型+5h+周 用量） | ✅（托盘菜单） |
| `usage_refresh_interval` | 30 | cc-switch SQLite 用量读取间隔（秒），0 关闭 | 重启 |
| `usage_5h_limit_requests` | 50 | 5h 滚动窗口**用户消息数**上限（Anthropic Pro 真实配额；Max 改 250）；0 关闭百分比 | 重启 |
| `usage_week_limit_requests` | 1000 | 7d 滚动窗口用户消息数上限（Max 改 5000） | 重启 |

### 状态文件

| 路径 | 内容 |
|---|---|
| `%APPDATA%\Vpn_Monitor\geo_cache.json` | IP→Geo LRU 缓存（DiskFormat { entries, lru }） |
| `%APPDATA%\Vpn_Monitor\overlay_state.json` | 浮窗位置 + 锁定状态（拖动时刷盘） |
| `%APPDATA%\Vpn_Monitor\vpn-monitor.log` | 启用日志后写到这里（5MB 自动轮换） |
| `~/.cc-switch/cc-switch.db` (**只读外部依赖**) | cc-switch 写入；本工具读 `proxy_request_logs` 表做用量统计 |
| `~/.cc-switch/settings.json` (**只读外部依赖**) | cc-switch 写入；本工具读 `currentProvider*` 字段做多源检测 |
| `~/.claude/settings.json` (**只读外部依赖**) | Claude Code / cc-switch 写入；本工具读 `env.ANTHROPIC_MODEL` 拿当前 Claude 模型 |
