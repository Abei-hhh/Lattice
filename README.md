# Lattice

Windows 11 平台上的 IP 状态悬浮窗 + 系统监控工具，Rust 实现，零运行时依赖，单文件 exe。
后台轮询公网 IP / 归属地 / 系统资源，悬浮条置顶常驻显示。通过托盘菜单、全局热键、可视化设置对话框进行交互。

## 功能特性

### 浮窗显示
- 屏幕顶部半透明悬浮条，**两种形态可切换**：
  - **简易（simple）**：双行布局，总高 ~56px
  - **完整（detailed）**：三行布局，总高 ~120px，第三行全宽展示流量曲线 + 国家分布
- **第一行**：当前 AI 工具标签 + 状态点 + IP 地址 + 城市 + 延迟 + ⚠ 跨源警告 + 代理节点名 / [v6 泄漏] / [DNS 泄漏]
- **第二行可切换两种模式**：
  - **system**：↑/↓ 网速 + CPU + 内存（默认）
  - **usage**：主用模型 + 5小时配额% + 倒计时 + 7天配额% + 倒计时（格式 `5小时:26% 41m / 7天:56% 1d1h`；百分比按**真实用户消息数**算 —— 解析 `~/.claude/projects/**/*.jsonl` 里 `type:user` 且不含 `tool_use_id` 的行，对齐 Anthropic 限流与 cc-switch UI 口径；颜色按 utilization：绿/橙/红）
- **第三行**（仅 detailed）：国家分布堆叠条 + top-3 legend + 全宽双折线流量曲线（近 60 个采样点）
- 窗口宽度自适应内容，每行内容水平居中；长文字自动截断
- **可拖动**：默认未锁定时左键按住浮窗任意位置可拖到屏幕任意角落；位置自动持久化
- **位置记忆**：`%APPDATA%\Lattice\overlay_state.json` 保存窗口位置 + 锁定状态
- **多显示器 / 高 DPI**：按所在显示器工作区居中，响应 `WM_DPICHANGED` / `WM_DISPLAYCHANGE`
- **周期性强制置顶**：3 秒重新断言 `HWND_TOPMOST`，对抗全屏程序 / UAC 抢占

### 托盘图标 + 多层右键菜单
所有可切换的开关都收纳在二级子菜单里，顶层只保留频繁用到的动作项：

```
显示设置 ▸    显示浮窗 / 锁定位置 / 鼠标穿透
浮窗形态 ▸    简易 / 完整 (含流量曲线)
第二行模式 ▸  系统资源 / AI 用量
隐私 & 缓存 ▸ 日志掩码 IP / 日志掩码归属地 / 启用归属地缓存 / HTTPS 跨源校验
────────────
IP 查询...
历史时间线...
用量明细...
────────────
高级设置...
文件 ▸       打开 config.toml / 打开日志目录
────────────
退出
```

所有 ☑ 项立即生效，不需要重启。

### IP / 归属地探测
- **多源 IP 抓取**：并发 ipify / ip.sb / ifconfig.me，任一成功立即返回
- **双源归属地**：ipwho.is（HTTPS）+ ip-api.com（HTTP）并发查询
- **HTTPS 跨源校验**：两源国家不一致时浮窗显示 ⚠ 橙色警告，**抵御中间人篡改 HTTP 响应伪造归属地**
- **归属地 LRU 磁盘缓存**：按 /24 子网键缓存 7 天，切回常用 VPN 节点城市瞬间显示
- **失败原因分类**：超时 / DNS 失败 / 无连接 / TLS 错误 / HTTP 状态码 / 限流 / 私有段 等
- 网络异常时浮窗仍保留上次已知 IP + 城市（暗色），只让状态点变红
- 连续失败自动指数退避；唤醒 / 代理变化自动触发即时重查

### 系统监控
- CPU 全局使用率
- 内存总用量百分比
- 网络速率（上传 / 下载 KB/s）
- **多层代理检测**：注册表 `ProxyEnable` / PAC URL → 已知代理进程名 → 代理专用端口（并发扫描）
- **代理变化即时联动**：检测到代理翻转立即触发 IP 重查
- **空闲降频**：用户键鼠空闲 ≥ 阈值时所有轮询间隔 × 5

### AI 工具标签（cc-switch 多源）
浮窗左上 tag 可显示以下任意 AI CLI 当前选中的 provider 名：
- **Claude**（优先级最高，会读 `~/.claude/settings.json` env.ANTHROPIC_MODEL）
- Codex / Gemini / OpenCode / Hermes / OpenClaw

在"高级设置..."→ 高级 tab 用 radio 切换源，**立即生效**。

### AI 用量统计（cc-switch SQLite 集成）
**前提**：已安装 [cc-switch](https://github.com/farion1231/cc-switch) 并启用代理模式，请求会自动落库到 `~/.cc-switch/cc-switch.db`。

- 浮窗第二行切到 "AI 用量" 模式 → 实时显示主用模型 + 5h/周请求数 + 费用
- 托盘 → **用量明细...** → 完整窗口：4 时段 radio（5h/24h/7d/30d）+ 按 provider×model 分组的 ListView
  - 列：工具 / Provider / 模型 / 请求数 / 输入Tok / 输出Tok / 费用 / 平均延迟
  - 按费用降序排序

### Clash / Mihomo / sing-box 节点名集成
本地代理工具如果暴露了 [Clash API](https://clash.gitbook.io/doc/restful-api)（默认端口 9090），浮窗第一行会用绿色 `→ {节点名}` 替代默认的"未设置代理"文本。
- 支持 Clash / Clash-Meta / Mihomo / sing-box（带 clash-api 兼容层）
- 自动探测端口 9090 / 9001 / 6170
- 每 5 秒刷新一次

### DNS + IPv6 泄漏检测
后台每 2 分钟做三路并发探测：
- **v4 国别**：复用主 IP 轮询结果
- **v6 国别**：调 `api6.ipify.org` 强制走 v6 路径
- **DNS 国别**：调 `https://1.1.1.1/cdn-cgi/trace` 提取 Cloudflare 看到的 DNS 解析者位置

如果三者国别不一致，浮窗第一行末尾会显示红色 `[v6泄漏]` / `[DNS泄漏]` 徽章。

### 流量分流可视化（完整形态）
启用完整形态后，浮窗右侧 sparkline 卡片顶部多一条 6px 国家分布堆叠条：
- 用 Win32 `GetExtendedTcpTable` 拿当前所有活跃 TCP 连接的远端 IPv4
- 配合 `geo_cache` 反查国家 → 按比例堆叠（颜色按国家名 hash → HSV）
- 每 10 秒扫描一次
- **回答"我的流量到底有多少出墙"** —— 不依赖 in-app proxy，纯系统级 TCP 表观察

### 主题
- `system` / `light` / `dark` 三选一
- `system` 跟随 Win11 个性化设置（`AppsUseLightTheme`），用户切了暗色后浮窗也跟着变
- 对话框标题栏走 `DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`
- 历史窗口 ListView 走 `SetWindowTheme(DarkMode_Explorer)`
- 设置对话框可视化切换，立即生效

### 历史时间线窗口
- 浏览 GeoCache 累积的所有 IP→Geo 记录（按时间倒序）
- 搜索框实时过滤（IP / 国家 / 城市 / ISP / 网段任一命中）
- 双击行 → 以该 IP 打开 lookup 对话框重查（验证缓存是否过期）
- 右键 → 复制 IP / 复制完整行 / 从缓存删除
- 一键导出 CSV（UTF-8 BOM，Excel 直接打开）

### 高级设置对话框
5-Tab：**常规 / 网络 / 隐私&安全 / 热键 / 高级**，覆盖 20+ 配置字段。
保存时用 `toml_edit` **保留 config.toml 中的所有注释和顺序**。

### 安全 / 隐私
- **日志 IP 掩码**：默认 `1.2.x.x` 形式，浮窗仍显示完整 IP
- **日志归属地脱敏**：默认 FNV-1a hash 替换（`geo:xxxxxxxx`）
- **代理 URL 凭证脱敏**：日志中 `socks5://user:pass@host` 自动变 `socks5://***@host`
- **HTTPS 优先 + 跨源警告**：见上文"跨源校验"

### 稳定性
- 单实例守卫：命名 mutex + FindWindow 双重检查，二次启动自动激活已有实例
- 监控线程 `catch_unwind` 兜底，最多自动重启 10 次
- 启动期失败用 `MessageBoxW` 提示而非闷退
- Mutex 中毒自动 `into_inner()` 恢复

## 快捷键

| 快捷键 | 功能 |
|--------|------|
| `Ctrl+Alt+H` | 切换浮窗显示/隐藏 |
| `Ctrl+Alt+I` | 打开 IP 查询工具窗口 |
| `Ctrl+Alt+Shift+K` | 退出程序 |

均可在"高级设置 → 热键"tab 改键（需重启生效）。

## 编译

需要 [Rust](https://rustup.rs/) 工具链（MSVC target）+ Windows SDK（用于 `embed-resource` 调用 `rc.exe` 编译 .rc 资源）。

```bash
cargo build              # 调试编译
cargo build --release    # 发布编译（约 2.5MB 单文件 exe）
```

编译产物：`target/release/lattice.exe`。
`build.rs` 在编译期把 `assets/app.svg` 光栅化为多尺寸 .ico 并嵌入 exe 资源段。

## 使用方法

1. 双击 `lattice.exe` 运行
2. 屏幕顶部出现悬浮条 + 系统托盘出现盾形图标
3. 通过托盘右键菜单或全局热键交互

## 配置文件

路径：与 `lattice.exe` 同目录的 `config.toml`，首次启动自动生成。

**编辑方式**（推荐顺序）：
1. 托盘 → "高级设置..." 可视化编辑（保留注释，部分字段立即生效）
2. 托盘 → "打开 config.toml" 直接编辑文本（需重启生效）

### 主要字段（完整列表见 `CLAUDE.md`）

| 字段 | 默认 | 立即生效？ |
|---|---|---|
| `theme` | "system" | ✅ |
| `opacity` | 0.85 | ✅ |
| `click_through` | false | ✅ |
| `mask_ip_in_log` | true | ✅ |
| `mask_geo_in_log` | true | ✅ |
| `geo_cache_enabled` | true | ✅ |
| `geo_cross_check` | true | ✅ |
| `active_cc_switch_provider` | "claude" | ✅ |
| `overlay_form` | "simple" | ✅（托盘菜单） |
| `row2_mode` | "system" | ✅（托盘菜单） |
| `usage_refresh_interval` | 30 秒 | 重启 |
| `usage_5h_limit_requests` | 50（Pro）/ 250（Max） | 重启 |
| `usage_week_limit_requests` | 1000（Pro）/ 5000（Max） | 重启 |
| `check_interval` | 10 秒 | 重启 |
| `timeout` | 5 秒 | 重启 |
| `idle_threshold_seconds` | 900 | 重启 |
| `hotkey_*` | ctrl+alt+h/i/shift+k | 重启 |

### 状态文件

| 路径 | 内容 |
|---|---|
| `%APPDATA%\Lattice\geo_cache.json` | IP→Geo LRU 缓存（本工具写入） |
| `%APPDATA%\Lattice\overlay_state.json` | 浮窗位置 + 锁定状态（本工具写入） |
| `%APPDATA%\Lattice\lattice.log` | 启用日志后写到这里（5MB 轮换，本工具写入） |
| `~/.cc-switch/cc-switch.db` | cc-switch 写入；本工具只读做用量统计 |
| `~/.cc-switch/settings.json` | cc-switch 写入；本工具只读做多源探测 |
| `~/.claude/settings.json` | Claude Code / cc-switch 写入；本工具只读拿 `env.ANTHROPIC_MODEL` |

## 退出方式

- 托盘右键 → 退出
- 快捷键 `Ctrl+Alt+Shift+K`
- 命令行 `taskkill /IM lattice.exe`

## 技术栈

- **语言**：Rust 2021
- **Workspace 双 crate**：
  - `lattice-core`（lib）—— 平台无关，Linux/macOS 上 `cargo build -p lattice-core` 也能编
  - `lattice`（binary）—— Windows 桌面 GUI 壳
- **GUI**：Win32 API 直调（`windows-rs` crate，无 winit / egui 依赖）
- **HTTP**：`reqwest` + `rustls` TLS
- **异步**：`tokio` 多线程运行时（IP/Geo/RPC/泄漏检测）+ 独立 OS 线程跑系统监控和 TCP 表扫描
- **渲染**：GDI 原生 + DWM 圆角 + ClearType + 自绘 MD3 owner-draw 按钮 + 双折线 sparkline
- **图标**：编译期 SVG → resvg/usvg → 多尺寸 PNG → 手写 ICO → embed-resource 链接
- **SQLite**：`rusqlite` bundled（只读 cc-switch.db 做用量统计，零系统依赖）
- **配置**：`toml` 读 + `toml_edit` 写（保留注释）
- **系统监控**：`sysinfo`（增量进程刷新）+ Win32 `GetExtendedTcpTable`（流量分流）

## 文档

- [`CLAUDE.md`](./CLAUDE.md) — 架构 / 设计决策 / 完整配置字段表 / 状态文件 / 后台 task 一览
- [`PORTING.md`](./PORTING.md) — 多平台适配方案（Linux/macOS/Android/iOS）+ 已落地 / 候选 roadmap
- [`DESIGN.md`](./DESIGN.md) — 早期设计文档（部分内容已被 CLAUDE.md 替代）
