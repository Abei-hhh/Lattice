# Vpn_Monitor

Windows 11 平台上的 IP 状态悬浮窗 + 系统监控工具，Rust 实现，零运行时依赖，单文件 exe。
后台轮询公网 IP / 归属地 / 系统资源，悬浮条置顶常驻显示。通过托盘菜单、全局热键、可视化设置对话框进行交互。

## 功能特性

### 浮窗显示
- 屏幕顶部半透明悬浮条，双行布局
- **第一行**：当前 AI 工具标签 + 状态点 + IP 地址 + 城市 + 延迟 + ⚠ 跨源警告 + 代理状态
- **第二行**：上传/下载网速 + CPU 使用率 + 内存使用率
- 窗口宽度自适应内容，每行内容水平居中；长文字自动截断
- **可拖动**：默认未锁定时左键按住浮窗任意位置可拖到屏幕任意角落；位置自动持久化
- **位置记忆**：`%APPDATA%\Vpn_Monitor\overlay_state.json` 保存窗口位置 + 锁定状态
- **多显示器 / 高 DPI**：按所在显示器工作区居中，响应 `WM_DPICHANGED` / `WM_DISPLAYCHANGE`
- **周期性强制置顶**：3 秒重新断言 `HWND_TOPMOST`，对抗全屏程序 / UAC 抢占

### 托盘图标 + 两层右键菜单
| 类型 | 项 |
|---|---|
| **快速开关**（立即生效） | 显示浮窗 / 锁定位置 / 鼠标穿透 / 日志掩码 IP / 日志掩码归属地 / 启用归属地缓存 / HTTPS 跨源校验 |
| **动作** | IP 查询... / 历史时间线... |
| **更多** | 高级设置... / 打开 config.toml / 打开日志目录 / 退出 |

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

编译产物：`target/release/vpn-monitor.exe`。
`build.rs` 在编译期把 `assets/app.svg` 光栅化为多尺寸 .ico 并嵌入 exe 资源段。

## 使用方法

1. 双击 `vpn-monitor.exe` 运行
2. 屏幕顶部出现悬浮条 + 系统托盘出现盾形图标
3. 通过托盘右键菜单或全局热键交互

## 配置文件

路径：与 `vpn-monitor.exe` 同目录的 `config.toml`，首次启动自动生成。

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
| `check_interval` | 10 秒 | 重启 |
| `timeout` | 5 秒 | 重启 |
| `idle_threshold_seconds` | 900 | 重启 |
| `hotkey_*` | ctrl+alt+h/i/shift+k | 重启 |

### 状态文件

| 路径 | 内容 |
|---|---|
| `%APPDATA%\Vpn_Monitor\geo_cache.json` | IP→Geo LRU 缓存 |
| `%APPDATA%\Vpn_Monitor\overlay_state.json` | 浮窗位置 + 锁定状态 |
| `%APPDATA%\Vpn_Monitor\vpn-monitor.log` | 启用日志后写到这里（5MB 轮换） |

## 退出方式

- 托盘右键 → 退出
- 快捷键 `Ctrl+Alt+Shift+K`
- 命令行 `taskkill /IM vpn-monitor.exe`

## 技术栈

- **语言**：Rust 2021
- **GUI**：Win32 API 直调（`windows-rs` crate，无 winit / egui 依赖）
- **HTTP**：`reqwest` + `rustls` TLS
- **异步**：`tokio` 多线程运行时（IP/Geo 查询）+ 独立 OS 线程跑系统监控
- **渲染**：GDI 原生 + DWM 圆角 + ClearType + 自绘 MD3 owner-draw 按钮
- **图标**：编译期 SVG → resvg/usvg → 多尺寸 PNG → 手写 ICO → embed-resource 链接
- **配置**：`toml` 读 + `toml_edit` 写（保留注释）
- **系统监控**：`sysinfo`（增量进程刷新）

## 文档

- [`CLAUDE.md`](./CLAUDE.md) — 架构 / 设计决策 / 完整配置字段表 / 状态文件
- [`DESIGN.md`](./DESIGN.md) — 早期设计文档（部分内容已被 CLAUDE.md 替代）
