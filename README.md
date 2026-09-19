# GameReady

一键就绪 · 开局即战 —— Windows 游戏配置便携工具。免安装单 exe，配置内嵌，拷走即用。

## 功能

- **LOL（WeGame 国服）配置守护**：点击大按钮开启，后台每 200ms 将内置配置循环覆盖到游戏目录（对抗客户端回写），再点关闭
- **Steam 一键覆盖**：将内置配置一键覆盖到**当前登录账号**（顶部徽标实时显示覆盖进度；可选全局配置一起覆盖，覆盖时自动退出并重启 Steam）
- **Steam 账号管理**（内嵌面板）：本地账号列表，登录/删除；登录优先历史登录态免密直登，失效时自动用保存的凭据代输（`Data\accounts.json`，自用工具明文）；新增登录 = 登录 + 自动覆盖配置一步到位（支持等待 SteamGuard 验证码）
- **进程活跃指示**：左列游戏图标右上角绿点脉冲 = 对应客户端正在运行
- **覆盖日志**：全部操作留痕（exe 同级 `Data\logs.jsonl`，时间降序，5000 条自动截断）
- **WebView2 引导**：无 WebView2 的机器启动时弹原生小窗自动/手动安装，装好直接进入（无需重启）
- 单实例防多开、系统托盘、开机自启、便携数据（全部存 exe 同级 `Data\`，拷文件夹即迁移）

## 环境要求

- Rust（stable，`x86_64-pc-windows-msvc`）：<https://rustup.rs>
- MSVC 构建工具：Visual Studio 2022 Build Tools（"使用 C++ 的桌面开发"工作负载，含 Windows SDK）
- 前端零构建（纯静态 HTML，无需 Node）

## 构建

```bash
build.bat           # 构建并把 gameready.exe 复制到项目根目录
build.bat clean     # 同上，并在构建后清空 target（只留根目录 exe）
```

（等价于 `cargo build --release` + 复制 `target\release\gameready.exe`；产物约 10 MB 单文件便携）

直接双击 exe 运行；首次运行在 exe 同级自动创建 `Data\`（设置/日志/凭据）。

## 目录结构（小而精：Rust 单文件 / 前端单文件 / 文档单文件）

```
├── src/main.rs      全部 Rust（单文件 ~2400 行，按分节注释划分：
│                     入口/托盘 → store → logger → watcher → packs →
│                     cover → guard → steam → guide(WebView2引导) → commands）
├── frontend/index.html  全部前端（单文件，CSS/JS 内联，window.__TAURI__ 全局 API）
├── packs/           内嵌配置资产（编译期打进 exe，勿改名）
│   ├── lol/…        LOL 5 文件（Game\Config 三件套 + LeagueClient\Config 两个 yaml）
│   └── steam/…      Steam 2 文件（config.vdf 全局 / localconfig.vdf 账号级）
├── DESIGN.md        设计文档合订本（需求台账 R1–R23 / 覆盖映射 / 路径调研 /
│                     WebView2 引导设计 / command 与事件速查）
├── build.bat        一键构建（exe 直出根目录）
├── Cargo.toml / build.rs / tauri.conf.json / capabilities/ / icons/
```

## 内置配置的更新

配置包在 `packs/` 下，编译期嵌入。修改/替换其中的文件后重新 `cargo build --release` 即可。
覆盖后的文件时间戳统一为 `2000-01-01 00:00:00`，便于识别哪些文件是本程序写入的。

## 开发约定与接口速查

- 源码单文件分节结构：改哪个功能看 `src/main.rs` 对应分节注释；需求演进史见 git log
- 弹窗全部自定义（无系统 alert/confirm）；tooltip 全部 `data-tip`（无系统白条）
- "当前登录账号" = Steam 正在运行且 AutoLoginUser 指向的账号（历史登录不算）
- `.cargo/config.toml` = 国内 crates 镜像（rsproxy），保证 clone 后依赖可下载，勿删
- command：`get_status / set_paths(lolRoot,steamRoot) / get_settings / set_settings(settings) / open_data_dir / start / stop / steam_cover(includeGlobal) / accounts_list / switch_account(sid64) / delete_account(sid64) / login_new(account,password) / list_logs(filter)`
- 事件：`game-active{lol,steam} / guard-stats{count,elapsed_ms} / guard-error{message} / cover-done(CoverReport) / log-appended(LogEntry) / steam-accounts-changed`
