# DSH Desktop 阶段 2：兼容运行时、首次安装与更新实施计划

> 依据：已批准的桌面设计、阶段 1 接口，以及
> `docs/research/2026-08-21-deepseek-harness-runtime.md` 的官方一手调研。
> 实施方式：Subagent-Driven Development；每个任务先 RED、再最小实现、复核、验证、独立提交。

## 目标与边界

本阶段让 Windows 10/11 x64 个人测试版能够下载、验证、安装并启动固定兼容版本的
Node 24 + `@deepseek-ai/dsh` Web runtime；发现官方 npm 新版时只通知，只有项目签名的
兼容清单允许安装。更新在隔离数据副本上探活，成功后才切换；首启失败时 runtime 与
数据指针成对回滚。

不包含沉浸皮肤、NSIS 安装器、卸载器、WebView2 Bootstrapper 或桌面壳自动更新。
不提交 runtime 大文件、私钥或真实用户数据。

### 建议新增的固定生产依赖

首次新增依赖时执行一次 `cargo add` 并提交 `Cargo.lock`，不得手改锁文件：

- `reqwest = =0.13.4`：HTTPS、JSON、流式下载；关闭不需要的默认特性；
- `tokio = =1.53.1`：异步文件写入和超时，复用 Tauri async runtime；
- `futures-util = =0.3.34`：带大小上限的响应流；
- `sha2 = =0.10.9`：runtime 包 SHA-256；
- `semver = =1.0.28`：官方版本与兼容约束；
- `url = =2.5.8`：结构化校验 HTTPS artifact URL，拒绝凭据、fragment 与伪装 scheme；
- `ed25519-dalek = =3.0.0`：兼容清单 detached signature 验证；
- `zip = =8.4.0`，仅 `deflate`：解压 runtime 包并执行 Zip Slip 防护。
- `tauri-plugin-notification = =2.3.3`：Windows 系统通知、点击恢复窗口；只在 Task 11
  接入最小 capability，不向远程 DSH 页面开放通知命令。

私钥只由发布环境通过参数或 secret store 提供。桌面程序只内置/注入公钥；开发测试
使用测试夹具密钥，不允许把生产私钥放入仓库。

## Task 1：版本化目录与 active 状态契约

**Files:** modify `src-tauri/src/paths.rs`, create `src-tauri/src/runtime/install_state.rs`,
modify `src-tauri/src/runtime/mod.rs`。

**Interfaces:** `RuntimeLayout`, `ActiveDeployment`, `InstalledRuntime`, `DataGeneration`,
`InstallStateStore`。

1. 先写测试：版本只接受严格 semver；版本目录必须留在 `runtimes/dsh/<version>`；
   数据 generation 必须留在漫游目录 `dsh-home/generations/<id>`；deployment JSON
   未存在、截断、未知 schema、任一路径逃逸分别返回类型化错误。
2. Run RED：`cargo test --manifest-path src-tauri/Cargo.toml install_state`。
3. 最小实现：单一 `deployment.json` 同时保存 schema、runtime version/relative dir、
   data generation relative dir、manifest digest、activation timestamp；先创建完整目录，
   再将临时文件 flush 后在漫游设置目录内原子 replace，从而一次提交 runtime+data 配对。
   文件不保存绝对下载 URL。
4. 不创建或删除旧 runtime；安装状态读写与进程启动解耦。
5. Run：`cargo test --manifest-path src-tauri/Cargo.toml install_state`，
   `cargo test --manifest-path src-tauri/Cargo.toml paths`，
   `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`。
6. Commit：`feat: define versioned runtime state`。

## Task 2：扩展真实 `RuntimeLaunchSpec`

**Files:** modify `src-tauri/src/runtime/command.rs`, `src-tauri/src/runtime/process.rs`,
`src-tauri/src/runtime/mod.rs`, `src-tauri/tests/lifecycle.rs`。

**Interfaces:** 为 `RuntimeLaunchSpec` 增加 `cwd: PathBuf`；新增
`RuntimeLaunchSpec::official(node, cli, cwd, dsh_home, port)`、`RuntimeOutputSink` 和
`ReadinessSignal`/`ReadinessPolicy`。

1. 先写测试验证固定 argv：`lib/bin.js web --host 127.0.0.1 --port <dynamic> --no-open`；
   program/args/cwd/env 都是独立值，空格和中文路径不经 shell。
2. 写失败测试：node、CLI、cwd 非绝对路径或不存在时拒绝；CLI 必须位于选中 runtime。
3. 修改 process spawn 使用 `.current_dir(&spec.cwd)`；mock 构造器显式使用安全 cwd。
4. 为 stdout/stderr 启动受管 drain 线程，使用固定上限缓冲并在停止时 join；只把精确的
   `dsh web: http://127.0.0.1:<port>` 解析为 readiness，其他 stdout 不持久化，stderr
   只上报脱敏后的错误类别，避免 pipe 填满卡死真实 DSH 或泄露用户正文。
5. 保留 `RuntimeSupervisor`、Job Object、HTTP health probe，不从新代码直接 spawn；
   official policy 要求 stdout readiness 与 HTTP 根页都成功，阶段 1 mock policy 仍只
   要求 HTTP，避免测试夹具伪造官方输出。
6. Run：`cargo test --manifest-path src-tauri/Cargo.toml command`，
   `cargo test --manifest-path src-tauri/Cargo.toml --test lifecycle`。
7. Commit：`feat: build official DSH launch specs`。

## Task 3：兼容清单、摘要与签名验证

**Files:** create `src-tauri/src/update/manifest.rs`, `src-tauri/src/update/mod.rs`,
create `src-tauri/tests/fixtures/runtime-manifest/`（仅小型文本夹具）。

**Interfaces:** `CompatibilityManifest`, `RuntimeArtifact`, `ManifestVerifier`。

1. 先写测试覆盖 schema、DSH/Node/桌面最低版本、platform=`windows`、arch=`x86_64`、
   size、SHA-256、HTTPS URL、验证时间、兼容摘要。
2. detached signature 对原始 manifest bytes 验证，公钥按固定 32-byte hex 解码；测试
   错 key、篡改字节、错长度、未知 schema、HTTP URL 全部失败。
3. 版本与 digest 比较使用解析后的类型，不做字符串排序；错误不得回显整份 manifest。
4. Run：`cargo test --manifest-path src-tauri/Cargo.toml manifest`。
5. Commit：`feat: verify signed compatibility manifests`。

## Task 4：可复现的 Windows runtime 包制作

**Files:** create `scripts/build-runtime.ps1`, `scripts/sign-runtime.mjs`,
`runtime/manifest.schema.json`, `runtime/locks/dsh-0.1.1-rc.1/package.json`,
`runtime/locks/dsh-0.1.1-rc.1/package-lock.json`, create `docs/runtime-release.md`,
modify `.gitignore`。

**Interfaces:** PowerShell 参数必须包含 exact DSH version、Node archive、Node SHA-256、
输出目录；签名是独立发布步骤，不从脚本读取仓库内私钥。

1. 先写 Pester-free dry-run/参数测试：缺版本、非 x64 Node、摘要不符、npm 解析到不同
   版本都失败；禁止 `latest`、`npx`、全局 npm 安装。
2. 每个兼容 DSH 版本先审查并提交完整 `package-lock.json`；使用独立 staging 解压官方
   Node 24 x64，再复制 lock 并执行 `npm ci --prefix <stage> --omit=dev`，禁止在发布时
   重新解析传递依赖；核对安装后的 package name/version 与 lock root。
3. 运行 `node .../lib/bin.js --help` 和短时 Web smoke；生成依赖 licenses/notices、
   文件清单和 SHA-256；压缩包不含 npm cache、用户目录或发布密钥。
4. `sign-runtime.mjs` 从 `DSH_RUNTIME_SIGNING_KEY_FILE` 指定的外部 PEM 读取私钥，生成
   manifest 原始 bytes 与 detached signature；测试夹具执行 sign -> Rust verify ->
   篡改失败闭环。生产私钥路径、内容和 signature 不写日志。
5. `.gitignore` 只忽略明确的 `runtime-out/`，不使用宽泛 runtime 通配规则。
6. Run：`pwsh -NoProfile -Command "$tokens=$null; $errors=$null;
   [System.Management.Automation.Language.Parser]::ParseFile((Resolve-Path
   'scripts/build-runtime.ps1'),[ref]$tokens,[ref]$errors) | Out-Null;
   if($errors.Count){exit 1}"`，`node --check scripts/sign-runtime.mjs`，
   `cargo test --manifest-path src-tauri/Cargo.toml runtime_release_fixture`，
   `git check-ignore runtime-out/probe.zip`。
7. Commit：`build: add compatible runtime packaging`。

## Task 5：受限 HTTPS 下载与原子暂存

**Files:** create `src-tauri/src/update/download.rs`, modify `src-tauri/src/update/mod.rs`。

**Interfaces:** `ArtifactDownloader` trait、`HttpsDownloader`、`DownloadPolicy`、
`DownloadedArtifact`。

1. 用本地 mock HTTP server 写 RED 测试：成功、连接超时、响应超时、429/5xx 有界重试、
   无 Content-Length、声明过大、流中超限、断开、摘要错误。
2. 仅允许 HTTPS 生产 URL；测试 server 通过注入 transport，不在生产放宽 HTTP。
3. 下载到 `updates/<trace-id>/artifact.part`，边流边计数和 SHA-256；成功后 flush 并
   rename 为 `.verified`。失败不修改 active，不记录 query/auth header。
4. 重试只覆盖可恢复网络错误和 429/5xx，指数退避有总截止时间与可取消 token。
5. Run：`cargo test --manifest-path src-tauri/Cargo.toml download`。
6. Commit：`feat: download verified runtime artifacts`。

## Task 6：安全解压与安装目录封存

**Files:** create `src-tauri/src/update/archive.rs`, modify `src-tauri/src/update/mod.rs`。

**Interfaces:** `RuntimeArchiveInstaller`。

1. RED 夹具覆盖 `../`、绝对路径、盘符路径、反斜杠逃逸、symlink/reparse、重复文件、
   文件数/展开大小上限、损坏 zip、目标版本已存在；另覆盖 Windows 大小写碰撞、
   尾随点/空格、ADS 冒号、`CON`/`NUL`/`COM1` 等设备名和 file-vs-directory 前缀冲突。
2. 每个 entry 先规范化相对路径并验证仍位于 staging；Windows 不创建 symlink；
   解压在 blocking worker，失败留下结构化诊断但不激活。
3. 核对 `node.exe`、DSH `package.json`、`lib/bin.js`、版本、文件清单；完成后原子移动到
   immutable 版本目录。不得覆盖或静默删除旧目录。
4. Run：`cargo test --manifest-path src-tauri/Cargo.toml archive`。
5. Commit：`feat: install immutable runtime archives`。

## Task 7：官方版本发现与兼容通知分离

**Files:** create `src-tauri/src/update/version_source.rs`,
create `src-tauri/src/update/coordinator.rs`, modify `src-tauri/src/domain.rs`。

**Interfaces:** `OfficialVersionSource`、`CompatibilitySource`、`UpdateNotice`、
`UpdateCoordinator`。

1. RED 测试解析 npm registry `dist-tags`/exact version/integrity；GitHub 只作为说明；
   空结果、非法 semver、超时、限流、响应过大均结构化失败。
2. 官方新版但无签名兼容条目 => `OfficialAwaitingCompatibility`，绝不返回可安装动作；
   兼容清单验证成功 => `CompatibleAvailable`。
3. 通知按 channel+version 去重并持久化下次检查时间；启动后延迟检查、托盘默认 12h，
   不阻塞 UI，不把网络失败当“没有更新”。
4. URL 根通过构建配置/trait 注入，业务代码不硬编码 token；npm 包名允许固定为官方名。
5. Run：`cargo test --manifest-path src-tauri/Cargo.toml version_source`，
   `cargo test --manifest-path src-tauri/Cargo.toml coordinator`。
6. Commit：`feat: discover official and compatible updates`。

## Task 8：首次安装与隔离 `DSH_HOME` 探活

**Files:** create `src-tauri/src/update/probe.rs`, modify `src-tauri/src/app_controller.rs`,
create `src-tauri/tests/runtime_probe.rs`。

**Interfaces:** `RuntimeProbe`、`ProbeWorkspace`、`ProbeReport`。

1. RED 测试：全新空 generation 成功；真实 active generation 不被写；stdout readiness
   + HTTP 根页双门禁；
   超时、非零退出、配置加载失败、WebUI 无效、取消均回收进程树。
2. probe 不负责复制数据，只接收 activator 在停止当前 DSH 后创建的一致 candidate
   generation；全新安装由 activator 创建空 generation。candidate 位于漫游数据根，
   继承当前用户 ACL，不放在可随意清理的 updates cache。
3. Task 8 只验证 activator 已创建的 candidate；创建前空间/ACL 与快照职责属于 Task 9。
   probe 会再次检查大小上限、reparse/hardlink、敏感文件 DACL，并以不共享 DELETE 的
   目录句柄固定 candidate 身份。状态文件不写入 `DSH_HOME`，而是追加到固定的
   `generations/.state/<candidate>/` 元数据目录；schema 1 严格绑定 candidate id、runtime
   version、manifest digest、trace_id 与 state，同一次重试不得复用不同 trace 的 marker。
   `ProbeLease` 由 `AppController` 原子签发；Task 9 必须保留原 lease 直到 deployment
   提交完成，probe 内部的 clone 则覆盖完整 async 生命周期与最终状态落盘。
   candidate/旧 generation 明确标为 active、inactive 或 failed。阶段 2 不删除含凭据的
   generation，后续设置页清理必须列出精确目录并再次确认。
4. `ProbeReport` 只含版本、阶段、耗时、重试、错误类型、trace_id，不含 prompt/key。
5. Run：`cargo test --manifest-path src-tauri/Cargo.toml runtime_probe`。
6. Commit：`feat: probe compatible runtimes on isolated data`。

## Task 9：事务切换、运行中门禁与首启回滚

**Files:** create `src-tauri/src/update/activation.rs`, modify
`src-tauri/src/app_controller.rs`, `src-tauri/src/runtime/mod.rs`。

**Interfaces:** `RuntimeActivator`、`ActivationJournal`、`RuntimeBusyState`。

1. 状态机分别建模 runtime lifecycle 与 Agent busy：允许 `Idle + ConfirmedIdle` 直接
   安装，也允许 `Ready + ConfirmedIdle` 进入受控停止；拒绝 Starting、Stopping、
   ActiveTask 与 UnknownBusy。顺序固定为 stop -> candidate snapshot -> probe ->
   deployment replace -> start。
2. deployment 原子文件同时指向 runtime 与 data generation；activation journal 位于
   同一漫游设置目录，记录 prepared/committed。测试在目录完成、journal flush、pointer
   replace、首启各崩溃点恢复，不能出现新 runtime + 旧 data 的混合部署。
3. 新版首次真实启动失败，停止新版、将 deployment 指针恢复为旧 runtime+generation
   配对，再启动旧版；
   回滚失败进入明确 `RecoveryRequired`，不循环重试。
4. 首次安装单独建模 `FreshInstall`：没有 prior deployment 时创建空 generation；首个
   真实启动失败后回到 `Uninstalled`，保留已安装但 inactive 的 runtime/generation，
   不尝试启动不存在的旧版；测试离线第二次启动。
5. 已产生新版数据后的手动回退不自动覆盖，必须显示兼容风险并保留两份数据。
6. Run：`cargo test --manifest-path src-tauri/Cargo.toml activation`，
   `cargo test --manifest-path src-tauri/Cargo.toml app_controller`。
7. Commit：`feat: activate runtimes with rollback`。

## Task 10：更新日志与脱敏门禁

**Files:** create `src-tauri/src/diagnostics.rs`, modify update/runtime 调用点和
`src-tauri/src/tray.rs`, create `src-tauri/tests/diagnostics.rs`。

**Interfaces:** `DiagnosticEvent`（trace_id、stage、elapsed_ms、retry、pid、error_kind）。

1. RED 测试输入 API key、Authorization、URL query、用户正文、Unicode 路径；输出不得
   包含 secret/header/prompt，路径只保留必要 basename 或类别。
2. 替换本阶段新增的 `println!/eprintln!`；阶段 1 现有 UI 诊断一并接入同一结构化边界，
   不新增远程日志服务。
3. 文件日志大小有上限和滚动；写失败不导致更新状态机 panic。
4. Run：`cargo test --manifest-path src-tauri/Cargo.toml diagnostics`，再用 `rg` 检查
   update/runtime 不直接记录 env、headers、body。
5. Commit：`feat: add redacted runtime diagnostics`。

## Task 11：首次安装、更新确认与通知 UI

**Files:** modify `src/main.ts`, `src/style.css`, `src/runtime-events.ts`,
create/modify对应 Vitest，modify `src-tauri/src/lib.rs`。

**Interfaces:** 限定命令 `get_update_state`、`check_updates`、`install_compatible_update`、
`confirm_activation`；远程 DSH 页面不获得通用下载/文件/进程权限。

1. 前端 RED：未安装、检查中、官方待兼容、兼容可用、下载/验证/探活、等待重启、
   回滚、恢复必需；按钮忙碌时防重复，错误可重试。
2. 安装/切换需要明确用户确认并展示版本、大小、兼容摘要；官方待兼容状态没有
   “强制安装”按钮。
3. Tauri 事件订阅保持先 listen 后 snapshot 的竞态保护；状态文案用 `textContent`，
   不注入 release notes HTML。
4. 接入 `tauri-plugin-notification`，capability 只允许本地启动页发应用自有更新通知；
   通知设置默认开启、同 channel+version+state 只发一次。点击只恢复/聚焦本地窗口，
   不接受通知 payload 导航；进入 DSH 后仍禁止远程页面直接 invoke 更新命令。
5. Run：`pnpm test && pnpm build`，
   `cargo test --manifest-path src-tauri/Cargo.toml command_permissions`。
6. Commit：`feat: present compatible runtime updates`。

## Task 12：真实 Windows 个人测试版验收

**Files:** create `scripts/smoke-runtime.ps1`, modify `docs/development.md`,
modify `package.json`。

1. `smoke-runtime.ps1` 只读检查工具与已下载 fixture，不删除用户数据；自动覆盖：签名
   错误、摘要错误、断网、解压逃逸、probe 失败不改 active、首启失败回滚。
2. 手工矩阵：全新目录安装 rc、离线重启、官方新版通知与兼容通知分离、运行中拒绝
   切换、确认更新、托盘重启、回滚、单实例、退出后无 Node/DSH 残留。
3. 记录启动窗口可交互时间、DSH ready 时间、空闲/托盘 CPU、桌面/Node/DSH 内存；
   不达标则记录基线，不用隐藏轮询或降低探活规则规避。
4. 更新 `pnpm check` 纳入 Rust fmt/test/strict Clippy、前端测试/build 和小型安全夹具；
   大 runtime smoke 单独显式运行。
5. Run：`pnpm check`、`pwsh -File scripts/smoke-runtime.ps1 -Fixture <path>`、
   `git diff --check`、`git status --short`。
6. Commit：`test: verify compatible runtime updates`。

## 最终验收

1. 全新 `%APPDATA%/DSH Desktop` 与 `%LOCALAPPDATA%/DSH Desktop` 能安装签名兼容 runtime
   并启动官方 WebUI；第二次启动离线可用。
2. 下载、摘要、签名、解压、probe 任一步失败都不改变 active 或真实 DSH_HOME。
3. 官方 npm 新版与可安装兼容版分开通知；任意官方版本不能绕过签名清单。
4. runtime 与数据快照成对切换/回滚；活动任务或未知 busy 状态不切换。
5. 工作树干净；每项独立提交；没有 runtime 二进制、生产私钥、API Key、鉴权头或
   用户提示正文进入 Git/日志。

阶段 2 完成后才能规划阶段 3 皮肤；不得提前宣称已有安装器或桌面自动更新。
