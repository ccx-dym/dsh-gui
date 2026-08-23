# DSH Desktop 开发说明

本文面向贡献者与发布维护者。普通用户请阅读[中文使用指南](用户使用指南.md)。

## 环境要求

- Windows 10/11 x64；
- Node.js 24 LTS；
- pnpm 11；
- Rust stable `x86_64-pc-windows-msvc`；
- Visual Studio C++ Build Tools；
- Microsoft Edge WebView2 Runtime。

项目使用固定版本的前端与 Rust 依赖。不要混用 npm、Yarn 或其他 Rust 工具链，
也不要把开发机专用的 crates.io 镜像写入仓库。

## 安装与运行

```powershell
pnpm install
pnpm check
pnpm tauri dev
```

`pnpm check` 依次执行 Rust fmt、锁定依赖的完整 Rust 测试、将所有 target 警告视为
错误的 Clippy、前端测试与构建，以及小型 runtime 安全夹具。大 runtime ZIP 不进入
日常门禁，必须显式提供 fixture 单独执行。也可以运行以下只读烟雾入口，先核对开发命令和 WebView2，再
执行完整门禁：

```powershell
pwsh -File scripts/smoke-desktop.ps1
```

兼容 runtime 的自动安全验收覆盖签名错误、摘要错误、连接中断、解压逃逸、probe
失败不改 active、候选首启失败回滚，以及皮肤格式/尺寸、只读协议、版本回退和命令
ACL。`-SecurityFixturesOnly` 只使用仓库内的小型生成夹具，不依赖大图片或 runtime ZIP。
脚本只读 ZIP，并只在系统临时目录下创建
全新的审计目录；它不读取、写入或删除真实 `%APPDATA%/DSH Desktop`、
`%LOCALAPPDATA%/DSH Desktop Data`、NSIS 程序目录或 DSH_HOME。测试目录不会由脚本自动删除：

```powershell
pwsh -NoProfile -File scripts/smoke-runtime.ps1 `
  -Fixture runtime-out\build-peer-closure-20260821-r3\dsh-runtime-0.1.1-rc.1-node-24.15.0-win-x64.zip `
  -RuntimeDirectory runtime-out\build-peer-closure-20260821-r3\stage\dsh-0.1.1-rc.1-node-24.15.0-win-x64
```

`-RuntimeDirectory` 可选；提供时必须与 ZIP 使用相同 `inventory.json`，脚本会逐文件复核
闭包、大小和 SHA-256，再使用独立 `DSH_HOME` 启动真实官方 WebUI，并同时验证精确
stdout 就绪信号与 HTTP 200。runtime 目录本身保持只读，进程树在探活结束后回收。

## Windows RC 手工验收矩阵

必须使用全新的、专门用于本轮 RC 的数据目录和当前用户安装；不得把个人真实 DSH_HOME
作为验收输入。自动化结果不能替代以下 GUI、托盘、通知和进程树检查：

| 场景 | 操作与通过条件 | 实测 |
| --- | --- | --- |
| 全新安装 | 安装 RC，签名兼容 runtime 探活后进入官方 WebUI | `0.1.5` current-user NSIS 安装通过；`0.1.1-rc.2` 签名 runtime 已发布，完整 GUI 激活链路待 `0.1.6` 复测 |
| 离线重启 | 成功安装后断网重启，继续使用已激活 runtime，不要求下载 | 待实测 |
| 两类通知 | 官方 npm 新版与已签名兼容版分别显示；前者不出现安装确认 | 自动状态机与签名清单通过；真实通知交互待实测 |
| busy 拒绝 | 活动任务及 unknown busy 状态下确认更新，runtime/data 均不切换 | 待实测 |
| 冷启动确认 | 在线下载后二次确认；仅下次冷启动执行 probe 与成对切换 | 待实测 |
| 托盘重启 | 最小化到托盘后可恢复；托盘重启仍加载同一 active pair | 待实测 |
| 回滚 | 候选首启失败时恢复旧 runtime/data pair，旧 WebUI 可用 | 待实测 |
| 单实例 | 连续启动两次只保留一个桌面实例和一棵 DSH 进程树 | 通过：第二实例退出码 0，桌面进程始终为 1 |
| 完整退出 | 托盘退出后无该次安装所启动的 Node/DSH 残留 | 待实测 |

性能数据必须来自 Release/NSIS 安装后的真实进程，记录机器 CPU、内存、Windows 版本、
WebView2 版本及测量工具。不要通过隐藏额外轮询、放宽或降低探活规则来美化数据：

| 指标 | 测量口径 | 实测值 |
| --- | --- | --- |
| 窗口可交互时间 | 启动进程至主窗口可点击 | 主窗口句柄 241 ms；WebView 可点击时间待精确计时 |
| DSH ready 时间 | 启动进程至 stdout + HTTP 双门就绪 | 1673 ms（与 ZIP inventory 逐文件绑定后的真实 runtime 烟雾） |
| 前台空闲 CPU | WebUI 就绪后连续 60 秒平均值 | 0.000%（无 runtime 的本地安全态） |
| 托盘 CPU | 隐藏到托盘后连续 60 秒平均值 | 0.000% |
| 桌面内存 | `dsh-desktop.exe` Working Set / Private Bytes | 38.7 MiB / 14.4–14.5 MiB |
| Node/DSH 内存 | 受管 Node/DSH 进程树 Working Set / Private Bytes 合计 | 待完成 `0.1.6` 首次安装后测量 |

2026-08-22 本机 RC 环境：Windows 11 专业版 build 26200、AMD Ryzen 9 9950X3D、
32 逻辑处理器、95.6 GiB 内存、WebView2 151.0.4129.93。CPU 百分比按进程 CPU
时间差除以 60 秒和逻辑处理器数计算；0.000% 表示该采样精度下未观察到增量，不代表
理论上绝对为零。

## Phase 3 沉浸式皮肤 GUI 与性能验收记录

本节是每个 Phase 3 RC 都要复制填写的记录模板，不代表已经通过。测试必须使用 Release
NSIS 当前用户安装和真实 WebView2；选择专门用于测试的 PNG、JPEG、WebP，不记录源文件名
或完整路径。测试前记录安装包字节数、SHA-256、Authenticode 状态和独立审计目录；不要
运行卸载器、删除历史受管皮肤或清理脚本留下的审计目录。

### RC 与环境

| 项目 | 实测值 |
| --- | --- |
| RC 版本 / commit | `0.1.5` / 远端发布 commit `ee846eec1092a3722e7d94e364a714a8d01c29df`；`0.1.6` 待本轮发布 |
| 安装包字节数 / SHA-256 | `0.1.5`：4,474,884 / `A4DC201164155D3CDD406A7E727769C4393CE6C4EDB864DF499F844FCE88BE7B` |
| Tauri updater 签名 | `0.1.5` detached signature 与 stable 元数据独立验证通过；Windows Authenticode 代码签名未配置 |
| Windows 版本 / build | Windows 11 专业版 / 26200 |
| CPU / 逻辑处理器 / 内存 | AMD Ryzen 9 9950X3D / 32 / 95.6 GiB |
| WebView2 版本 | 151.0.4129.93 |
| DSH 版本 / 适配器状态 | `0.1.1-rc.2` runtime 与签名清单已发布；真实页面和皮肤适配器待 `0.1.6` GUI 激活后验收 |
| 安全烟雾审计目录 | `%TEMP%\dsh-desktop-runtime-smoke\14a548d4b12641328e419f252157553c`（已保留） |

### GUI 验收步骤

1. 从托盘打开“选择或设置皮肤”，确认只出现独立本地设置窗口，主 DSH 会话不中断。
2. 用原生选择器分别导入测试 PNG、JPEG、WebP；确认过滤器只列出四个扩展名
   `png/jpg/jpeg/webp`，预览不含源路径。导入后重命名或移走源文件，确认受管副本仍可用。
3. 逐一验证 cover、contain、stretch、center 和九个位置；确认页面始终只有一个背景合成层，
   模糊只影响背景，浅/深遮罩以及遮罩/面板透明度下正文和控件仍可读。
4. 保存后重启桌面端，确认设置和图片仍生效；恢复默认后确认官方视觉完整恢复，历史受管
   图片仍保留。
5. 分别选择错误格式、损坏文件、超过 20 MiB、边长超过 7680 或总像素超限的测试文件，
   记录稳定中文错误码/提示；不得把文件名或路径写入日志。
6. 在已验证 DSH 版本上检查聊天、流式输出、工具审批、文件选择器和终端交互；随后用不支持
   的版本或 DOM 合约验证“未验证”状态，确认皮肤节点与样式全部撤销，官方功能仍可用。
7. 关闭外观窗口应只隐藏该窗口；主窗口最小化到托盘、恢复、单实例和显式退出语义保持不变。

| GUI 场景 | 结果 | 证据/备注（脱敏） |
| --- | --- | --- |
| 托盘打开外观窗口与窗口生命周期 | 部分通过 | 主窗口关闭后进程继续运行且窗口隐藏；Computer Use 无法定位系统通知区域，托盘打开外观窗口仍待人工确认 |
| PNG/JPEG/WebP 导入与受管副本 | 待实测 | 待记录 |
| 四种填充、九个位置、背景专用模糊 | 待实测 | 待记录 |
| 遮罩/面板可读性与保存后重启 | 待实测 | 待记录 |
| 恢复默认且历史图片保留 | 待实测 | 待记录 |
| 无效、损坏、超大和超尺寸错误 | 待实测 | 待记录 |
| DSH 业务交互无回归 | 待实测 | 待记录 |
| 不支持版本/DOM 失败关闭回退 | 自动门禁通过 | 真实 DSH 页面待 `0.1.6` 激活后实测 |

### 默认视觉与 8K 皮肤性能对照

在同一台机器、同一 Windows/WebView2/DSH 构建上各测一次默认视觉和 8K 测试皮肤。
进程启动到可点击窗口及 DSH stdout + HTTP 双门分别计时；前台和托盘 CPU 均连续采样
60 秒。内存同时记录桌面进程及其 WebView2 进程组，滚动检查需确认没有重复图片解码或
明显卡顿。通过线：启动小于 5 秒、DSH ready 小于 10 秒、空闲 CPU 接近 0%。

| 指标 | 默认视觉 | 8K 皮肤 | 判定/备注 |
| --- | --- | --- | --- |
| 进程到可点击窗口（ms） | 待实测（主窗口句柄出现为 145 ms，不等同 DOM 可点击） | 待实测 | 需用可点击探针判定 `< 5000 ms` |
| DSH 双门 ready（ms） | 待实测 | 待实测 | `0.1.1-rc.2` 发布源已就绪，等待 GUI 首次安装验收 |
| 前台 60 秒平均 CPU | 2.057%（仅 desktop 主进程、单核口径） | 待实测 | WebView2 进程组 CPU 尚未合并 |
| 托盘 60 秒平均 CPU | 0.000%（desktop 主进程） | 待实测 | 默认视觉通过 |
| Desktop Working Set / Private Bytes | 40.51 / 15.10 MiB | 待实测 | 记录增量 |
| WebView2 进程组内存 | 512.21 / 328.13 MiB（WS / Private，8 进程） | 待实测 | 含隐藏本地窗口进程 |
| 滚动响应与重复解码 | 待实测 | 待实测 | 无明显卡顿/重复解码 |

完成手工验收后，再运行一次 `git diff --check` 与 `pnpm check`。只有自动门禁通过且上述
表格已填入真实证据，才可把该 RC 标记为 Phase 3 验收通过。

## RC 构建通道

真实 NSIS 构建必须由 CI 或隔离发布终端注入四个非敏感的
`DSH_DESKTOP_*` 必填变量。没有真实受控 HTTPS endpoint 或发布公钥时，构建可用于本地
界面验收，但更新状态必须显示明确的配置缺失状态（当前状态码
`release_configuration_unavailable`，即 configuration required），不得替换成硬编码测试
URL 或对外分发：

```powershell
$env:DSH_DESKTOP_NPM_REGISTRY_ROOT = 'https://registry.npmjs.org/'
$env:DSH_DESKTOP_COMPAT_MANIFEST_URL = 'https://raw.githubusercontent.com/ccx-dym/dsh-gui/main/releases/runtime/stable/manifest.json'
$env:DSH_DESKTOP_COMPAT_SIGNATURE_URL = 'https://raw.githubusercontent.com/ccx-dym/dsh-gui/main/releases/runtime/stable/manifest.sig'
$env:DSH_DESKTOP_COMPAT_PUBLIC_KEY = '<64位小写hex发布公钥>'
pwsh -NoProfile -File scripts/smoke-desktop.ps1 -RequireReleaseChannel
pnpm tauri build --bundles nsis
```

GitHub Raw 地址只有在 stable manifest 和签名已经发布后才可用于正式构建；公钥占位符
不可用于实际发布。`-RequireReleaseChannel` 会在任一编译期配置缺失时以
`release_channel_missing` 失败，避免产出无法安装 DSH 的 EXE。实际 CI 必须从已审核的发布配置注入
四个值，可另设 `DSH_DESKTOP_UPDATE_CHANNEL=stable`；这些变量不需要 token，也不得把生产
私钥、API Key 或鉴权头放入环境或构建日志。

NSIS 明确使用 `currentUser` 安装模式，不请求管理员权限；WebView2 使用小体积的官方
download bootstrapper。未配置 Tauri updater 私钥的个人 RC 必须显式关闭更新制品生成，
仍会产出可安装的普通 NSIS；该 EXE 会显示未知发布者，只能用于本机/小范围测试：

```powershell
pnpm tauri build --bundles nsis --config src-tauri/tauri.local-test.conf.json
```

## 桌面客户端独立更新发布

正式客户端更新由 `desktop-v<version>` tag 触发，并且要求 `package.json`、
`src-tauri/Cargo.toml` 和 `src-tauri/tauri.conf.json` 三处版本完全一致。GitHub
environment `desktop-release` 必须配置独立于 runtime 的以下值：

- secrets：`TAURI_SIGNING_PRIVATE_KEY`、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`；
- variables：`TAURI_UPDATER_PUBLIC_KEY`、`DSH_DESKTOP_UPDATE_ENDPOINT`；
- runtime 公开通道 variables：`DSH_DESKTOP_NPM_REGISTRY_ROOT`、
  `DSH_DESKTOP_COMPAT_MANIFEST_URL`、`DSH_DESKTOP_COMPAT_SIGNATURE_URL`、
  `DSH_DESKTOP_COMPAT_PUBLIC_KEY`。

推荐把 `DSH_DESKTOP_UPDATE_ENDPOINT` 配置为当前仓库的固定桌面通道：

```text
https://github.com/ccx-dym/dsh-gui/releases/download/desktop-stable/latest.json
```

工作流先运行完整门禁，再生成 current-user NSIS、Tauri `.exe.sig` 和只含
`windows-x86_64` 的 `latest.json`。上传前还会用客户端配置的公钥独立验证 EXE 与
detached signature，避免私钥和 GitHub 公钥变量不匹配。版本化 `desktop-v<version>` Release 不覆盖；
`desktop-stable` 只替换签名元数据 `latest.json`，其 URL 稳定。客户端仍会拒绝相同版本、
降级、无效元数据和签名不匹配的安装包。桌面更新与 runtime 更新使用不同密钥、状态文件
和命令权限，且安装前只有在下载及签名验证成功后才停止受管 DSH 进程。

脚本最后会打印五项 Windows 手工检查。手工测试日志只记录阶段、错误类型、耗时、
PID 和必要的 trace ID，不得包含 API Key、完整鉴权头或用户提示正文。

若 crates.io 在当前网络不可达，可通过 Cargo 的命令级 `--config` 临时指定镜像；
不要在仓库或用户目录写入持久镜像配置。无论使用哪一来源，都必须保留锁文件和
`clippy --all-targets -- -D warnings`，不得通过降低规则绕过诊断。

运行 `pnpm tauri dev` 后逐项确认：

1. 启动页出现，并在模拟服务就绪后导航到 `Mock DSH Ready`；
2. 关闭主窗口只隐藏到系统托盘，模拟 Node 进程仍在运行；
3. 左键托盘图标可恢复并聚焦原窗口；
4. 再次启动应用不会创建第二个窗口或第二个模拟服务；
5. 选择托盘“退出”后，窗口及其受管模拟进程均退出。

## 当前阶段边界

仓库保留阶段 1 的 mock 服务，用于回归窗口、WebView、动态端口、进程树回收、托盘和
单实例。阶段 2 已实现兼容 runtime 的安全下载、隔离 probe 与事务激活；Phase 3 已实现
沉浸式皮肤的本地设置、版本门禁和失败关闭；当前用户 NSIS、独立桌面更新通道与
`0.1.1-rc.2` 签名 runtime 均已发布。真实首次安装、官方 WebUI、托盘和皮肤性能仍必须
以本轮 RC 构建及上述手工矩阵的实际证据为准，未实测项不得写成通过。

阶段 2 的正式 DSH 接入点是 Rust 层的 `RuntimeLaunchSpec`。运行时选择器只负责构造
经过校验的可执行文件、参数、工作目录和环境；启动、探活、停止与 Windows Job Object
托管必须继续经过 `RuntimeSupervisor`，不得从 Tauri 命令、托盘处理器或 WebView
绕过该边界直接创建子进程。
