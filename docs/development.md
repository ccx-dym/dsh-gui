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

`pnpm check` 依次执行前端测试、TypeScript 与 Vite 构建、Rust 测试，以及将警告视为
错误的 Clippy 检查。也可以运行以下只读烟雾入口，先核对开发命令和 WebView2，再
执行完整门禁：

```powershell
pwsh -File scripts/smoke-desktop.ps1
```

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

阶段 1 仅运行仓库内的 mock 服务，用于验证窗口、WebView、动态端口、进程树回收、
托盘和单实例行为；它不是可交付的真实 DSH，也尚未实现真实运行时下载、兼容更新、
皮肤或安装器。

阶段 2 的正式 DSH 接入点是 Rust 层的 `RuntimeLaunchSpec`。运行时选择器只负责构造
经过校验的可执行文件、参数、工作目录和环境；启动、探活、停止与 Windows Job Object
托管必须继续经过 `RuntimeSupervisor`，不得从 Tauri 命令、托盘处理器或 WebView
绕过该边界直接创建子进程。
