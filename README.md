# GameReady

Windows 游戏配置便携工具，支持 LOL 配置守护、LOL 与 Steam 账号管理，以及 NAS 账号备份。主程序为便携单 exe，已内置 WeGame 快速填入所需的 FakerInput 驱动安装包，首次使用可在程序内一键安装。

当前版本：**v0.1.24** · [下载 EXE](https://github.com/BigdataZjk/GameReady/releases/latest/download/gameready.exe) · [更新记录](https://github.com/BigdataZjk/GameReady/releases)

## 下载与使用

1. 点击[下载 EXE](https://github.com/BigdataZjk/GameReady/releases/latest/download/gameready.exe)，直接下载 `gameready.exe` 后运行；缺少 WebView2 时按提示安装。
2. 在路径设置中填写 LOL 或 Steam 安装目录，可只配置一个。
3. 按需使用以下功能，支持托盘运行和开机自启。

| 功能 | 用途 |
| --- | --- |
| LOL 配置守护 | 开启后持续应用内置游戏配置，默认帧率上限为 240 FPS |
| LOL 账号 | 明文查看账号和密码，新增、编辑、删除、收藏，独立 NAS 备份 |
| Steam 设置 | 合并全局与账号设置，共用项一致，专有项保留各自有效位置；默认打开库，关闭所有弹出通知及新品推广通知，保留好友消息提示音；开启叠加界面及其缩放，关闭浏览器选项卡恢复；操作时会退出并重启 Steam |
| Steam 账号 | 切换登录、新增账号、删除本机账号数据；历史账号在启动前应用设置，避免二次启动；首次新账号确认后应用设置并恢复登录 |
| 操作日志 | 查看操作结果和失败原因 |

登录进度会显示退出、配置、启动与等待确认的实际阶段。验证码或手机确认在 Steam 中完成；等待期间可重新选择账号，旧任务自动取消。首次新账号登录确认后会自动退出、应用配置并恢复登录；账号尚未写入 Steam 登录历史时，也能通过活动会话识别并显示。

账号列表中的目标账号在整个登录流程（包括等待验证）持续显示“登录中…”并禁用登录按钮，避免重复点击重新发起同一账号的登录；流程结束或取消后恢复。

Steam 的启动位置属于账号漫游设置；开启或关闭全局配置覆盖时，账号设置都会应用“库”。应用漫游设置时同时更新该文件的 Steam 云缓存记录，保留云端版本和其他文件记录，避免新账号的配置文件被忽略；Steam 配置使用真实修改时间。启动 Steam 时使用其安装目录，保证相对路径正确。

Steam 账号列表直接读取 exe 同级 `Data/accounts.json`，换电脑或移动程序后，无需在本机先登录 Steam 即可显示全部保存账号；没有 SteamID 或密码的记录也会显示。本机 Steam 历史用于补全昵称、SteamID 和有效登录时间。账号按最近登录时间从新到旧显示，无法恢复的旧时间仍显示未知。账号行悬停时从右侧滑出「登录」「删除」，键盘聚焦也可操作。界面禁用默认右键菜单，仅账号行右键提供「复制账号」「复制密码」；复制密码使用 Data 保存的密码，缺失时会提示补充。点击「删除」直接清理所选账号的本地记录，无确认弹窗。

点击账号前的星标可收藏或取消收藏；收藏状态随账号保存在 `Data/accounts.json`，迁移数据后仍然保留。

登录按钮前的「识别」会检查剪贴板中的单行账号密码，支持重复符号、空格分隔及「账号：… 密码：…」等格式；可识别时自适应显示，否则隐藏。点击仅填入账号和明文密码，登录需另点「登录」。

覆盖日志右上角的「清除日志」直接清理当前标签对应的分类，无确认弹窗；在「全部」下清理所有日志，在「LOL 守护」「Steam 覆盖」「账号切换」下仅清理对应分类。

## LOL 账号与备份

LOL 页以紧凑卡片显示账号和密码，新增账号自动放在列表最上方并滚回顶部，编辑和收藏不会改变顺序。点击星标可收藏或取消收藏，支持新增、编辑和删除账号。剪贴板含可识别的单行账号密码时，新增区显示「识别」按钮；点击仅填入，需再点「添加」保存。账号资料及收藏状态保存在 exe 同级 `Data/lol-accounts.json`，随 Data 迁移；不依赖本机游戏登录记录。

悬停账号行时，编辑、删除按钮之前会显示「填入」，编辑和删除位置不变。打开 WeGame 后点击「填入」，程序按当前进程定位，支持恢复最小化或隐藏的窗口；若停在「快捷安全登录」页，会识别底部「QQ 账号密码登录」入口并切换，确认密码框就绪后清空旧内容、填写账号和密码，不点击登录。页面识别使用 Windows 本机文字识别，仅处理底部切换文字，不上传图像；系统不支持识别或页面不匹配时提示手动切换。

当前网吧版实测：已在密码页时约 0.8–0.9 秒，需要自动切页时约 1.7 秒，耗时随客户端状态和内容长度变化。填写时会将 WeGame 切到前台、短暂移动鼠标，结束后恢复鼠标原位置；若检测到用户移动鼠标则立即停止，不强行复位。填写期间请勿操作鼠标键盘或切换窗口；按 Esc 可停止，结束后核对 WeGame 并手动登录。

受保护密码通过 FakerInput 0.1.1 x64 虚拟 HID 键盘填入。约 1 MB 的官方原版驱动安装包已内嵌在 `gameready.exe`，无需用户另外下载。首次点击「填入」发现组件缺失时，会显示用途、[官方下载备用链接](https://github.com/Ryochan7/FakerInput/releases/download/v0.1.1/FakerInput_Setup_0.1.1_x64.msi)和「安装内置组件」按钮；也可从「关于 → WeGame 填入组件 → 查看 / 安装」打开。点击安装后由 GameReady 释放并校验安装包、调用 Windows Installer，请确认 Windows 的管理员授权与驱动安装提示。取消或失败可重试，要求重启时会提示，不会自行重启；安装成功后再次点击账号「填入」。

驱动每台电脑只需安装一次，已安装且就绪时直接填入，不再弹窗或重复安装；换电脑时仍需由程序安装一次。组件可以通过 Windows「已安装的应用」卸载。缺组件、设备不兼容或被占用时，不会恢复或清空 WeGame 输入框；安装结束也不会自动发送账号密码。GameReady 不修改 WeGame 或系统保护。

[FakerInput 官方项目](https://github.com/Ryochan7/FakerInput)采用 MIT 许可且已归档；已验证当前客户端的完整输入、重复覆盖、移动及隐藏/最小化恢复，其他版本仍需实测。安装包签名发布者为 Ryodigi Solutions LLC，SHA-256 为 `4c0aefb7340051a91d606776243298b5cd1143ef5508bbae6800c474f9ed0840`。内置包在构建和安装前校验哈希、签名；安装不需要联网下载。完整许可随 EXE 内置，可在组件窗口展开查看，亦附于本文末尾。

「账号备份」打开 LOL 的备份设置和待同步列表，可填写远程文件路径、开关自动备份、测试路径、保存配置，以及单个账号同步和全部同步。LOL 与 Steam 共用「设置 → 账号备份」中的 SFTP 连接；LOL 默认文件为 `/GameReady/lol-accounts.txt`。自动备份默认关闭，开启后新增账号或修改密码会触发同步。

## 公共 SFTP 与游戏账号备份

在「设置 → 账号备份」统一填写服务器、端口、用户名和密码；此处的「测试连接」只验证公共 SFTP 连接。端口默认 `2022`，请按 NAS 实际设置修改。

在 Steam、LOL 页各自的「账号备份」中填写远程文件绝对路径、设置自动备份开关，并通过「测试路径」检查当前游戏目标文件的读写权限。例如 Steam 使用 `/GameReady/accounts.txt`，LOL 使用 `/GameReady/lol-accounts.txt`；两款游戏不能指向同一个文件。未使用且关闭自动备份的游戏可将路径留空，远程目录需提前创建。各游戏独立保存自己的配置，弹窗同时显示待同步账号及同步结果；需要修改公共连接时可跳转到设置页。以下为 Steam 的同步规则，LOL 的账号保存后即可自动备份。

| 操作 | 行为 |
| --- | --- |
| 补充密码 | 展开明文输入框，确认后仅保存密码，立即变为「同步」按钮 |
| 同步 | 直接上传该账号已保存的密码，无需先登录 Steam |
| 全部同步 | 上传所有有密码且未同步的账号，缺密码的账号留待补充 |
| 自动备份 | 默认关闭；开启后，普通登录或新增账号确认登录成功时自动同步 |

待同步列表只显示未同步账号，「待补充密码」「待验证」以标签显示；补充密码不会启动 Steam。同步结果显示在弹窗底部，自动同步也会主动提示；登录完成提示同时显示该账号的 NAS 备份状态。

手动同步不受自动备份开关限制。远程文件为 UTF-8 文本，每行 `账号----密码`；新账号插入第一行，已有账号更新密码并保留原位置，不排序，保留远程其他账号。批量新增逐个置顶，相同密码已同步后不重复上传。失败时保留待同步记录，修正并保存配置或重新测试当前游戏路径通过后继续重试。

## 数据与升级

- 设置、账号和日志均保存在 exe 同级 `Data` 目录。**所有密码输入框显示明文，LOL、Steam 和 NAS 密码也按明文保存在本地。**
- 迁移到自己的新电脑时，将 exe 和同级 `Data` 文件夹一起复制。启动、进入 Steam 页、窗口重新获得焦点时重新读取账号；Steam 页可见时每 2 秒自动刷新文件变化。账号显示和复制不依赖 Steam 安装路径，登录前仍需配置新机器上有效的 Steam 路径。
- `backup-settings.json` 统一保存公共 SFTP 连接、服务器指纹以及各游戏的备份文件路径和自动备份开关。`accounts.json`、`lol-accounts.json` 分别保存各游戏账号、待同步记录和回执。
- 首次使用公共备份配置时会读取原有连接和各自路径。原连接一致或仅配置一款游戏时可迁移；若两套连接冲突，会暂停自动备份并在公共设置中提示重新配置，避免把账号上传到错误服务器。
- 本版只读取 `version: 2` 的统一格式 `accounts.json`，程序不内置旧文件转换。新用户直接运行；v0.1.15 及更早版本的分散账号文件需要在升级前完成一次性转换，不能直接按旧格式沿用。格式损坏或版本不受支持时保留原文件并报错，避免覆盖数据。
- 启动后在后台检查 GitHub 最新正式 Release，之后每 6 小时检查一次；发现新版时，版本号旁持续显示「有最新版本 v…」。也可在关于页手动检查，点击更新会下载并校验官方 EXE，保存到 `Data/updates` 后打开文件所在目录。
- 升级前退出程序（包括托盘），替换 exe 并保留原 `Data` 目录。
- Release 只提供 EXE：`gameready.exe` 用于固定下载链接，同内容的版本化 EXE 用于程序检查更新；不提供 ZIP。只保留最新一个 Release；分享时不要附带自己的 `Data`。

## 从源码构建

需要 Rust stable、MSVC C++ 构建工具和 Windows SDK。前端为静态 HTML，无需 Node 构建。

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File build.ps1
```

每次打包都会更新桌面的 `gameready.exe`，文件名固定。替换前停止 GameReady 及其子进程并等待完全退出，退出失败则中止替换；替换后校验 SHA-256，保留旁边的 `Data`。同时生成根目录的 `gameready.exe`，并在 `release` 目录生成已清除编译机路径信息的固定名和版本化 EXE，不生成 ZIP。加 `-Clean` 可在构建后清理缓存。

`src` 放程序和测试，`assets` 放界面、图标及合并后的内置模板；`Data` 仍只在运行时生成。

## FakerInput 开源许可

MIT License

Copyright (c) 2021 Travis Nickles

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
