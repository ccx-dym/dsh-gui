# DSH Desktop 开发说明

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
失败不改 active，以及候选首启失败回滚。脚本只读 ZIP，并只在系统临时目录下创建
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
| 全新安装 | 安装 RC，签名兼容 runtime 探活后进入官方 WebUI | current-user NSIS 安装与未配置发布源安全态通过；在线安装待发布源 |
| 离线重启 | 成功安装后断网重启，继续使用已激活 runtime，不要求下载 | 待实测 |
| 两类通知 | 官方 npm 新版与已签名兼容版分别显示；前者不出现安装确认 | 待实测 |
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
| Node/DSH 内存 | 受管 Node/DSH 进程树 Working Set / Private Bytes 合计 | N/A：本轮安装包未配置发布源，未启动受管 runtime |

2026-08-22 本机 RC 环境：Windows 11 专业版 build 26200、AMD Ryzen 9 9950X3D、
32 逻辑处理器、95.6 GiB 内存、WebView2 151.0.4129.93。CPU 百分比按进程 CPU
时间差除以 60 秒和逻辑处理器数计算；0.000% 表示该采样精度下未观察到增量，不代表
理论上绝对为零。

## RC 构建通道

真实 NSIS 构建必须由 CI 或隔离发布终端注入四个非敏感的
`DSH_DESKTOP_*` 必填变量。没有真实受控 HTTPS endpoint 或发布公钥时，构建可用于本地
界面验收，但更新状态必须显示明确的配置缺失状态（当前状态码
`release_configuration_unavailable`，即 configuration required），不得替换成硬编码测试
URL 或对外分发：

```powershell
$env:DSH_DESKTOP_NPM_REGISTRY_ROOT = 'https://registry.npmjs.org/'
$env:DSH_DESKTOP_COMPAT_MANIFEST_URL = 'https://updates.example.invalid/stable/manifest.json'
$env:DSH_DESKTOP_COMPAT_SIGNATURE_URL = 'https://updates.example.invalid/stable/manifest.sig'
$env:DSH_DESKTOP_COMPAT_PUBLIC_KEY = '<64位小写hex发布公钥>'
pnpm tauri build --bundles nsis
```

上例中的 `.invalid` 和公钥占位符不可用于实际发布。实际 CI 必须从已审核的发布配置注入
四个值，可另设 `DSH_DESKTOP_UPDATE_CHANNEL=stable`；这些变量不需要 token，也不得把生产
私钥、API Key 或鉴权头放入环境或构建日志。

NSIS 明确使用 `currentUser` 安装模式，不请求管理员权限；WebView2 使用小体积的官方
download bootstrapper。个人 RC 可用 `pnpm tauri build --bundles nsis` 构建，但未配置
代码签名证书时生成的 EXE 会显示未知发布者，只能用于本机/小范围测试，不应公开分发。

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
单实例。阶段 2 已实现兼容 runtime 的安全下载、隔离 probe 与事务激活；真实安装器和
桌面自动更新仍必须以本轮 RC 构建和手工矩阵的实际证据为准，皮肤属于后续阶段。

阶段 2 的正式 DSH 接入点是 Rust 层的 `RuntimeLaunchSpec`。运行时选择器只负责构造
经过校验的可执行文件、参数、工作目录和环境；启动、探活、停止与 Windows Job Object
托管必须继续经过 `RuntimeSupervisor`，不得从 Tauri 命令、托盘处理器或 WebView
绕过该边界直接创建子进程。
