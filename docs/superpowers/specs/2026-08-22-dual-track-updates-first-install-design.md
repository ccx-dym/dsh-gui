# DSH Desktop 首次安装与双轨更新设计

## 1. 目标

让 Windows 10/11 x64 当前用户安装版保持精简，并在首次启动时安装官方
DeepSeek Harness（DSH）runtime。DSH runtime 与桌面客户端分别发现、发布、安装和回滚；
官方 DSH 新版以核心功能为优先，只有皮肤不兼容时仍允许升级并自动恢复官方界面。

首个真实发布目标固定为官方 npm `@deepseek-ai/dsh@0.1.1-rc.2`。用户不需要下载
DSH 源码、安装系统 Node.js 或手工运行 npm。

## 2. 非目标

- 不从官方仓库的 `master` 分支直接构建用户更新。
- 不在用户电脑上执行 `npm install`、`npx` 或原生模块编译。
- 不因皮肤适配失败阻止核心兼容的 DSH 更新。
- 不自动安装尚未通过核心兼容验证的官方版本。
- 不把签名私钥、GitHub token、API Key 或用户凭据写入仓库或安装包。
- 不把 runtime 打进精简 NSIS 安装包。

## 3. 版本与发布源

### 3.1 上游版本发现

官方 npm registry 是可安装版本的事实来源：

```text
https://registry.npmjs.org/@deepseek-ai%2Fdsh
```

GitHub `deepseek-ai/deepseek-harness` 的 tag、release 和提交只用于发布说明与辅助审计。
扫描任务每 12 小时运行一次，也支持手动触发。发现新版只创建候选验证，不直接授权用户安装。

### 3.2 项目发布仓库

项目使用公开仓库 `https://github.com/ccx-dym/dsh-gui` 发布客户端和 runtime。

客户端 Release：

```text
desktop-v<desktop-version>
├── DSH-Desktop_<desktop-version>_x64-setup.exe
├── DSH-Desktop_<desktop-version>_x64-setup.exe.sig
└── latest.json
```

DSH runtime Release：

```text
dsh-v<dsh-version>-windows
├── dsh-runtime-<dsh-version>-node-<node-version>-win-x64.zip
├── manifest.json
└── manifest.sig
```

稳定 runtime 通道使用仓库内固定地址：

```text
releases/runtime/stable/manifest.json
releases/runtime/stable/manifest.sig
```

清单中的 artifact URL 指向不可变的版本化 GitHub Release 资产。清单和签名可以更新，
ZIP 资产不得原地覆盖。

## 4. 双轨更新模型

### 4.1 DSH runtime 更新

DSH runtime 包含固定 Node 24 Windows x64、官方 `@deepseek-ai/dsh` 精确版本和完整生产依赖。
每个版本安装到不可变目录，活动指针单独持久化：

```text
%LOCALAPPDATA%\DSH Desktop Data\runtimes\
├── 0.1.1-rc.2\
├── <future-version>\
└── active.json
```

更新流程为下载、大小校验、SHA-256 校验、Ed25519 签名验证、安全解压、隔离数据探活、
冷启动切换。任何失败均保留旧 runtime 和旧活动指针。

### 4.2 桌面客户端更新

桌面客户端通过独立 Release 更新 Tauri EXE、启动适配、协议和皮肤适配器。客户端更新
不得覆盖 DSH runtime 或用户数据。桌面更新元数据和安装包使用独立签名密钥。

### 4.3 用户可见状态

客户端必须区分：

- `official_available`：官方存在新版，正在等待兼容验证；
- `runtime_available`：核心兼容 runtime 可安装；
- `desktop_required`：核心不兼容，需要先更新客户端；
- `skin_unverified`：核心兼容但皮肤未验证，仍允许更新；
- `up_to_date`：当前版本已是最新兼容版；
- `offline`：无法检查或首次下载，保留当前可用状态。

## 5. 兼容判定

### 5.1 核心兼容

候选 runtime 必须通过：

1. 精确包名、版本、npm integrity 和依赖闭包检查；
2. Node 引擎和 Windows x64 原生依赖检查；
3. CLI help/version smoke；
4. `web --host 127.0.0.1 --port <dynamic> --no-open` 启动；
5. stdout loopback readiness 与 HTTP 根页面双门；
6. 临时 `DSH_HOME` 的配置、会话和工作区基本加载；
7. 进程树停止、异常启动和下载中断回收；
8. 隔离数据副本验证，不直接写真实用户数据。

核心不兼容时不发布可安装 runtime，状态为 `desktop_required` 或继续验证；用户继续使用旧版。

### 5.2 皮肤兼容

皮肤适配由桌面客户端内置并绑定 DSH 精确版本和 DOM 合约。页面加载后再次检查 DOM 锚点、
CSS 变量和导航来源。

若核心兼容但皮肤不兼容：

- 允许安装和运行新版 DSH；
- 移除所有皮肤节点和样式；
- 显示“当前版本皮肤未验证，已恢复官方界面”；
- 不授予远程页面设置、文件或更新权限。

## 6. 首次启动

全新安装没有活动 runtime 时，主界面不得显示 `retry_failed` 或“重新启动”。流程为：

```text
解析签名稳定清单
→ 展示 DSH 0.1.1-rc.2、下载大小和验证摘要
→ 用户确认“安装 DSH”
→ 下载并显示进度
→ 校验、解压和隔离探活
→ 写入待激活状态
→ 冷启动激活
→ 打开真实 DSH WebUI
```

断网时显示“连接网络后安装”，并提供重试。取消、断网、摘要错误、签名错误、磁盘不足、
探活失败均不得产生已安装假状态。已经安装过 runtime 时，离线重启不依赖发布源。

## 7. GitHub Actions

### 7.1 `scan-upstream.yml`

- `schedule` 每 12 小时和 `workflow_dispatch`；
- 读取 npm dist-tag、版本、tarball 和 integrity；
- 与已知候选和已发布 runtime 比较；
- 新版只创建候选 issue/workflow dispatch，不自动签名发布。

### 7.2 `build-runtime.yml`

- 输入精确 DSH 版本；
- 要求仓库已提交并审查对应 `runtime/locks/dsh-<version>`；
- 使用固定 Node archive 和官方 SHA-256；
- 构建 runtime、生成 inventory 和第三方 notices；
- 执行核心兼容门禁；
- 通过 environment approval 后签名并发布版本化 Release；
- 更新稳定清单与 detached signature。

### 7.3 `release-desktop.yml`

- 执行完整 Rust、前端、安全和安装器门禁；
- 构建 current-user NSIS；
- 使用独立桌面更新私钥签名；
- 发布安装包、签名和 `latest.json`；
- 不修改 runtime 稳定清单。

## 8. 密钥与权限

使用两套 Ed25519 密钥：

- runtime key：只授权兼容 runtime 清单；
- desktop key：只授权桌面客户端更新。

私钥仅存于 GitHub Actions environment secrets，发布 job 需要人工 environment approval。
仓库和客户端只保存公钥。Actions 权限最小化为 `contents: read`；发布 job 单独授予
`contents: write`，扫描 job 不具有写 Release 的权限。任何日志不得输出私钥路径、私钥、
GitHub token 或签名输入中的敏感环境变量。

## 9. 错误与恢复

- 网络失败：保留当前 runtime；首次安装显示离线状态。
- 版本元数据异常：不安装，不回显不可信正文。
- 清单或签名错误：稳定失败关闭，不接受 artifact。
- 下载中断：临时文件不成为活动版本，可重试。
- 解压逃逸、reparse、硬链接或 inventory 不一致：拒绝候选。
- probe 失败：候选不激活，保留诊断类别和审计目录。
- 新 runtime 首启失败：runtime 与数据 generation 成对回滚。
- 客户端更新失败：保留已安装客户端、runtime 和用户数据。

## 10. 测试与验收

### 10.1 自动门禁

- npm 新版发现与去重；
- `official_available`、`runtime_available`、`desktop_required`、`skin_unverified` 决策；
- 首次安装无 runtime 的明确 UI；
- runtime 下载大小、摘要、签名、连接中断和路径边界；
- 隔离 probe、事务激活、冷启动切换和回滚；
- 皮肤不兼容时仅清理皮肤，核心 WebUI 继续运行；
- 客户端 updater 签名、平台、版本和回滚边界；
- capability 保证远程 DSH 页面不能调用安装或更新命令。

### 10.2 Windows 实机验收

1. 全新用户目录安装精简 NSIS；
2. 首次启动下载并安装 `0.1.1-rc.2`；
3. 配置模型、选择工作区并运行真实任务；
4. 断网重启继续使用已安装 runtime；
5. 模拟核心兼容新版，只更新 DSH；
6. 模拟皮肤不兼容新版，DSH 可用且皮肤自动关闭；
7. 模拟核心不兼容新版，阻止 DSH 更新并提示更新客户端；
8. 更新客户端后重新开放对应 DSH runtime；
9. 下载中断、错误签名和启动失败保持旧版本；
10. 卸载客户端默认保留 DSH 数据，重装后可重新下载 runtime 并复用数据。

## 11. 分阶段交付

1. 为 `0.1.1-rc.2` 生成、审查和提交 runtime lock；
2. 建立密钥、公钥配置和 GitHub Actions runtime 发布链；
3. 发布首个 rc.2 runtime 与稳定签名清单；
4. 接通首次安装 UI 和真实 GitHub 通道；
5. 实机验证可用 DSH WebUI；
6. 接入并发布独立桌面客户端 updater；
7. 启用上游定时扫描和未来版本候选验证。

