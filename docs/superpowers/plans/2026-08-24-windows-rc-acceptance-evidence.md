# Windows RC Acceptance Evidence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 DSH Desktop 增加只读、隔离、可复核的 Windows RC 验收与证据采集入口，并生成不会把未执行人工项误报为通过的 JSON/Markdown 报告。

**Architecture:** 保留现有 `smoke-desktop.ps1` 的开发门禁职责，新增独立的入口脚本。采集逻辑和报告逻辑分别放入两个可 dot-source 的 PowerShell 文件；入口只负责边界校验、编排和创建全新审计目录，测试通过受控对象与短生命周期子进程覆盖纯逻辑和 Windows 进程采样。

**Tech Stack:** PowerShell 7、Windows CIM/Registry、.NET `System.Security.Cryptography`、现有 pnpm/Rust/Vitest 门禁。

**Spec:** `docs/superpowers/specs/2026-08-24-windows-rc-acceptance-evidence-design.md`

## Global Constraints

- 仅支持 Windows 10/11 x64，使用 PowerShell 7。
- 不新增生产依赖或开发依赖。
- 不安装、卸载、终止或重启真实 DSH Desktop、Node 或 WebView2 进程。
- 不修改网络、代理、防火墙、注册表、电源设置或用户 DSH 数据。
- 不读取或记录进程命令行、环境变量、窗口标题、聊天内容、URL 或 API Key。
- 不使用任何删除 API；每次测试和验收都创建并保留全新审计目录。
- `passed` 必须存在已执行证据；未执行或缺少外部条件只能是 `not_run`。
- 公共 PowerShell 函数参数使用明确类型，并以中文注释说明业务边界和失败原因。
- 所有 native command 非零退出必须失败关闭；动态异常正文不得进入报告。

---

### Task 1: 固定验收状态与报告 schema

**Files:**
- Create: `scripts/rc-acceptance/report.ps1`
- Create: `scripts/tests/rc-acceptance-report.Tests.ps1`

**Interfaces:**
- Consumes: 设计文档定义的 `passed`、`failed`、`not_run` 状态。
- Produces: `New-RcCheckList`, `Get-RcOverallResult`, `ConvertTo-RcMarkdown`, `Write-RcEvidenceFiles`。

- [ ] **Step 1: 编写状态收敛与人工项默认值失败测试**

在 `scripts/tests/rc-acceptance-report.Tests.ps1` 中 dot-source 尚不存在的报告模块，并验证
固定检查 ID、默认状态和顶层收敛：

```powershell
$ErrorActionPreference = 'Stop'
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
. (Join-Path $repositoryRoot 'scripts\rc-acceptance\report.ps1')

$checks = @(New-RcCheckList)
$expectedIds = @(
    'first_install', 'offline_restart', 'notification_split', 'busy_rejection',
    'tray_lifecycle', 'existing_pair_rollback', 'complete_exit', 'skin_import',
    'skin_layout', 'skin_persistence', 'adapter_fallback', 'scale_layout',
    'default_performance', 'skin_8k_performance', 'windows_10',
    'webview2_bootstrapper', 'authenticode'
)
if (@($checks.id) -join ',' -cne $expectedIds -join ',') {
    throw 'manual_check_ids_mismatch'
}
if (@($checks | Where-Object status -cne 'not_run').Count -ne 0) {
    throw 'manual_checks_must_default_not_run'
}
if ((Get-RcOverallResult -Checks $checks) -cne 'not_run') {
    throw 'not_run_must_propagate'
}

$passed = @([ordered]@{ id = 'a'; status = 'passed'; evidence = 'fixed evidence' })
if ((Get-RcOverallResult -Checks $passed) -cne 'passed') {
    throw 'all_passed_must_pass'
}
$failed = @($passed + [ordered]@{ id = 'b'; status = 'failed'; evidence = 'fixed evidence' })
if ((Get-RcOverallResult -Checks $failed) -cne 'failed') {
    throw 'failed_must_dominate'
}
```

- [ ] **Step 2: 运行测试并确认因模块缺失失败**

Run:

```powershell
pwsh -NoProfile -File scripts/tests/rc-acceptance-report.Tests.ps1
```

Expected: FAIL，指出 `scripts/rc-acceptance/report.ps1` 不存在。

- [ ] **Step 3: 实现固定检查清单和严格状态收敛**

在 `scripts/rc-acceptance/report.ps1` 中加入：

```powershell
Set-StrictMode -Version Latest

$script:RcStatuses = @('passed', 'failed', 'not_run')

function New-RcCheckList {
    [CmdletBinding()]
    param()

    $definitions = @(
        @('first_install', '在线首次安装并进入官方 WebUI'),
        @('offline_restart', '断网后冷启动继续使用已激活 runtime'),
        @('notification_split', '官方新版与兼容新版通知分离'),
        @('busy_rejection', 'busy 与 unknown busy 拒绝 runtime 切换'),
        @('tray_lifecycle', '托盘隐藏、恢复和重启保持同一实例'),
        @('existing_pair_rollback', '已有 active pair 时候选失败回滚'),
        @('complete_exit', '显式退出后受管进程树无残留'),
        @('skin_import', 'PNG、JPEG、WebP 导入与错误图片拒绝'),
        @('skin_layout', '填充、位置、透明度、遮罩和面板可读性'),
        @('skin_persistence', '保存重启与恢复默认'),
        @('adapter_fallback', '不支持 adapter 时撤销皮肤'),
        @('scale_layout', '100%/125% 缩放与最大化布局'),
        @('default_performance', '默认视觉性能采样'),
        @('skin_8k_performance', '8K 皮肤性能与滚动响应'),
        @('windows_10', 'Windows 10 实机验收'),
        @('webview2_bootstrapper', 'WebView2 缺失设备补装'),
        @('authenticode', '安装包 Authenticode 有效')
    )
    foreach ($definition in $definitions) {
        [ordered]@{
            id = $definition[0]
            description = $definition[1]
            status = 'not_run'
            evidence = ''
        }
    }
}

function Get-RcOverallResult {
    [CmdletBinding()]
    param([Parameter(Mandatory)][object[]] $Checks)

    foreach ($check in $Checks) {
        if ($check.status -cnotin $script:RcStatuses) {
            throw 'rc_status_invalid'
        }
    }
    if (@($Checks | Where-Object status -ceq 'failed').Count -gt 0) { return 'failed' }
    if ($Checks.Count -eq 0 -or
        @($Checks | Where-Object status -ceq 'not_run').Count -gt 0) { return 'not_run' }
    return 'passed'
}
```

- [ ] **Step 4: 增加 JSON/Markdown 脱敏与 CreateNew 写入失败测试**

扩展测试，构造包含固定 build/environment/installer/process 字段的 evidence；断言 Markdown
包含每个检查 ID 和状态，不包含测试安装包绝对路径、`CommandLine`、`API_KEY`；在同一目录
第二次写入必须以 `rc_evidence_exists` 失败，原文件 bytes 不变。

```powershell
$auditRoot = Join-Path ([System.IO.Path]::GetTempPath()) "dsh-rc-report-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $auditRoot | Out-Null
$evidence = [ordered]@{
    schema_version = 1
    generated_at_utc = '2026-08-24T00:00:00Z'
    result = 'not_run'
    build = [ordered]@{ desktop_version = '0.1.13'; git_commit = ('a' * 40) }
    environment = [ordered]@{ windows = 'Windows test'; webview2 = '1.2.3.4' }
    installer = [ordered]@{ file_name = 'setup.exe'; size_bytes = 4; sha256 = ('0' * 64); authenticode = 'NotSigned' }
    process_observation = [ordered]@{ status = 'not_run' }
    checks = @(New-RcCheckList)
}
Write-RcEvidenceFiles -Evidence $evidence -AuditDirectory $auditRoot
$jsonPath = Join-Path $auditRoot 'evidence.json'
$markdownPath = Join-Path $auditRoot 'report.md'
$before = [System.IO.File]::ReadAllBytes($jsonPath)
$markdown = [System.IO.File]::ReadAllText($markdownPath)
if ($markdown -notmatch 'offline_restart.*not_run') { throw 'markdown_check_missing' }
if ($markdown -match [regex]::Escape($auditRoot) -or $markdown -match 'CommandLine|API_KEY') {
    throw 'report_contains_sensitive_field'
}
try {
    Write-RcEvidenceFiles -Evidence $evidence -AuditDirectory $auditRoot
    throw 'duplicate_write_must_fail'
} catch {
    if ($_.Exception.Message -cne 'rc_evidence_exists') { throw }
}
if (-not [System.Linq.Enumerable]::SequenceEqual($before, [System.IO.File]::ReadAllBytes($jsonPath))) {
    throw 'existing_evidence_changed'
}
```

- [ ] **Step 5: 实现固定字段 Markdown 和排他文件写入**

实现 `ConvertTo-RcMarkdown`，只读取 schema 中批准的固定字段；实现内部
`Write-RcCreateNewUtf8`，使用 `FileMode.CreateNew`、UTF-8 no BOM 和最终 LF 写入
`evidence.json`/`report.md`。捕获 `IOException` 后只抛出固定错误 `rc_evidence_exists`，不得
拼接路径或动态异常正文。

- [ ] **Step 6: 运行报告测试并提交**

Run:

```powershell
pwsh -NoProfile -File scripts/tests/rc-acceptance-report.Tests.ps1
git diff --check
```

Expected: PASS。

Commit:

```powershell
git add scripts/rc-acceptance/report.ps1 scripts/tests/rc-acceptance-report.Tests.ps1
git commit -m "test: define RC acceptance evidence schema"
```

### Task 2: 采集构建、环境与安装包证据

**Files:**
- Create: `scripts/rc-acceptance/measurement.ps1`
- Create: `scripts/tests/rc-acceptance-measurement.Tests.ps1`

**Interfaces:**
- Consumes: 仓库根路径、明确的安装包文件路径。
- Produces: `Get-RcBuildEvidence`, `Get-RcEnvironmentEvidence`, `Get-RcInstallerEvidence`。

- [ ] **Step 1: 编写三处版本一致性和安装包元数据失败测试**

测试在新的临时仓库夹具中写入 `package.json`、`Cargo.toml`、`tauri.conf.json`，并创建名为
`fixture.exe` 的四字节普通文件：

```powershell
$ErrorActionPreference = 'Stop'
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
. (Join-Path $repositoryRoot 'scripts\rc-acceptance\measurement.ps1')

$fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) "dsh-rc-measure-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path (Join-Path $fixtureRoot 'src-tauri') | Out-Null
[System.IO.File]::WriteAllText((Join-Path $fixtureRoot 'package.json'), '{"version":"0.1.13"}')
[System.IO.File]::WriteAllText((Join-Path $fixtureRoot 'src-tauri\Cargo.toml'), "[package]`nversion = `"0.1.13`"`n")
[System.IO.File]::WriteAllText((Join-Path $fixtureRoot 'src-tauri\tauri.conf.json'), '{"version":"0.1.13"}')
$build = Get-RcBuildEvidence -RepositoryRoot $fixtureRoot -GitCommit ('a' * 40)
if ($build.desktop_version -cne '0.1.13' -or $build.git_commit -cne ('a' * 40)) {
    throw 'build_evidence_mismatch'
}

$installer = Join-Path $fixtureRoot 'fixture.exe'
[System.IO.File]::WriteAllBytes($installer, [byte[]](0, 1, 2, 3))
$artifact = Get-RcInstallerEvidence -Installer $installer
if ($artifact.file_name -cne 'fixture.exe' -or $artifact.size_bytes -ne 4 -or
    $artifact.sha256 -cne '054edec1d0211f624fed0cbca9d4f9400b0e491c43742af2c5b0abebf0c990d8') {
    throw 'installer_evidence_mismatch'
}
if ($artifact.PSObject.Properties.Name -contains 'path') { throw 'installer_path_must_not_escape' }
```

再把 Cargo 版本改为 `0.1.12`，断言固定错误 `desktop_version_mismatch`；把扩展名改为 `.zip`
或目标改成目录，断言固定错误 `installer_invalid`。

- [ ] **Step 2: 运行测试并确认模块缺失**

Run:

```powershell
pwsh -NoProfile -File scripts/tests/rc-acceptance-measurement.Tests.ps1
```

Expected: FAIL，指出 measurement 模块不存在。

- [ ] **Step 3: 实现严格构建版本解析**

实现：

```powershell
function Get-RcBuildEvidence {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string] $RepositoryRoot,
        [Parameter(Mandatory)][ValidatePattern('^[0-9a-f]{40}$')][string] $GitCommit
    )
    # package/tauri 用 ConvertFrom-Json；Cargo 只在 [package] 到下一个 section 内匹配唯一 version。
    # 三值必须完全一致，否则抛 desktop_version_mismatch。
}
```

读取时对三个配置文件设置 1 MiB 上限，拒绝缺失、重复 Cargo package version、未知类型和
非 exact semver；输出只包含 `desktop_version` 与 `git_commit`。

- [ ] **Step 4: 实现受文件锁保护的安装包证据**

`Get-RcInstallerEvidence` 必须先验证普通 `.exe` 文件且无 reparse 属性，再以
`FileShare.Read` 打开。使用 `SHA256.HashData(stream)` 计算小写摘要；文件锁保持期间调用
`Get-AuthenticodeSignature`，把状态映射到固定枚举 `Valid`、`NotSigned`、`HashMismatch`、
`NotTrusted`、`UnknownError`。输出只包含文件名、长度、摘要和固定签名状态。

- [ ] **Step 5: 实现只读系统与 WebView2 采集**

实现 `Get-RcEnvironmentEvidence`：

- 从 `Win32_OperatingSystem` 读取 caption/version/build number；
- 从 `Win32_ComputerSystem` 读取逻辑处理器数和物理内存；
- 从设计中的 HKCU/HKLM WebView2 客户端键读取 `pv`，多个值时排序后取精确非空值；
- WebView2 未发现时写固定值 `not_detected`，不写注册表路径；
- 输出不包含机器名、用户名、SID、序列号或安装路径。

- [ ] **Step 6: 增加环境对象字段白名单测试并运行**

断言输出属性恰好为：

```powershell
@('windows_caption', 'windows_version', 'windows_build',
  'logical_processors', 'physical_memory_bytes', 'webview2_version')
```

Run:

```powershell
pwsh -NoProfile -File scripts/tests/rc-acceptance-measurement.Tests.ps1
git diff --check
```

Expected: PASS。

- [ ] **Step 7: 提交构建与环境采集**

```powershell
git add scripts/rc-acceptance/measurement.ps1 scripts/tests/rc-acceptance-measurement.Tests.ps1
git commit -m "feat: collect bounded RC build evidence"
```

### Task 3: 指定 PID 的只读进程树与性能采样

**Files:**
- Modify: `scripts/rc-acceptance/measurement.ps1`
- Modify: `scripts/tests/rc-acceptance-measurement.Tests.ps1`

**Interfaces:**
- Consumes: 调用者指定的根 PID、5–300 秒采样时长。
- Produces: `Get-RcProcessSample`, `Compare-RcProcessSamples`, `Measure-RcProcessTree`。

- [ ] **Step 1: 编写纯进程树边界和指标比较失败测试**

用显式 rows 测试内部 `Select-RcProcessTree`，证明只包含根 PID 和递归后代，不包含同名旁系
进程，并能终止循环父子关系：

```powershell
$rows = @(
    [pscustomobject]@{ ProcessId = 10; ParentProcessId = 1; Name = 'dsh-desktop.exe' },
    [pscustomobject]@{ ProcessId = 11; ParentProcessId = 10; Name = 'msedgewebview2.exe' },
    [pscustomobject]@{ ProcessId = 12; ParentProcessId = 11; Name = 'node.exe' },
    [pscustomobject]@{ ProcessId = 20; ParentProcessId = 1; Name = 'node.exe' }
)
$tree = @(Select-RcProcessTree -Rows $rows -RootProcessId 10)
if (@($tree.ProcessId) -join ',' -cne '10,11,12') { throw 'process_tree_boundary_failed' }
```

构造前后 sample，断言 CPU 使用率除以采样秒数和逻辑处理器数，退出进程进入
`exited_process_ids`，新增后代进入 `new_process_ids`，缺失内存不补零。

- [ ] **Step 2: 运行测试并确认函数缺失**

Run:

```powershell
pwsh -NoProfile -File scripts/tests/rc-acceptance-measurement.Tests.ps1
```

Expected: FAIL，指出 `Select-RcProcessTree` 不存在。

- [ ] **Step 3: 实现无命令行字段的递归后代选择**

`Select-RcProcessTree` 只接受 `ProcessId`、`ParentProcessId`、`Name` rows。使用
`HashSet[int]` 记录已访问 PID，从根开始按父 PID 扩展；最终按 PID 排序。根 PID 不存在时
抛固定错误 `desktop_process_missing`。

- [ ] **Step 4: 实现单次进程采样和纯比较函数**

`Get-RcProcessSample` 使用 `Get-CimInstance Win32_Process` 获得关系，再只对选中 PID 调用
`Get-Process -Id`。每行仅保留：

```text
process_id, parent_process_id, process_name,
total_processor_time_ms, working_set_bytes, private_bytes
```

`Compare-RcProcessSamples` 接受 before/after、实际秒数和逻辑处理器数，生成根进程、全部
后代、`msedgewebview2`、`node` 四组聚合，以及新增/退出 PID。CPU 公式为：

```text
100 * max(0, after_cpu_ms - before_cpu_ms)
    / (elapsed_seconds * 1000 * logical_processors)
```

仅在前后均存在进程时计算 CPU；内存只聚合 after 中存活且数值有效的进程。

- [ ] **Step 5: 实现有界真实采样包装器**

```powershell
function Measure-RcProcessTree {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][ValidateRange(1, [int]::MaxValue)][int] $DesktopProcessId,
        [ValidateRange(5, 300)][int] $ObservationSeconds = 60
    )
    $before = Get-RcProcessSample -DesktopProcessId $DesktopProcessId
    $started = [System.Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Seconds $ObservationSeconds
    $after = Get-RcProcessSample -DesktopProcessId $DesktopProcessId -AllowMissingRoot
    $started.Stop()
    Compare-RcProcessSamples -Before $before -After $after `
        -ElapsedSeconds $started.Elapsed.TotalSeconds `
        -LogicalProcessors ([Environment]::ProcessorCount)
}
```

`-AllowMissingRoot` 只用于第二次采样，以便把退出记录为证据；第一次采样必须存在。

- [ ] **Step 6: 使用受控 PowerShell 子进程做 5 秒集成采样**

测试启动一个只执行 `Start-Sleep -Seconds 20` 的隐藏 `pwsh` 进程，将其 PID 传给
`Measure-RcProcessTree -ObservationSeconds 5`。断言根 PID 一致、报告不包含 CommandLine、
Path、窗口标题或环境变量。测试结束时调用 `CloseMainWindow()`；若测试进程仍运行，不主动
删除或终止，由 20 秒固定寿命自行退出，避免测试代码包含强制终止逻辑。

- [ ] **Step 7: 运行测试并提交**

Run:

```powershell
pwsh -NoProfile -File scripts/tests/rc-acceptance-measurement.Tests.ps1
git diff --check
```

Expected: PASS，执行时间不少于 5 秒且不超过 15 秒。

Commit:

```powershell
git add scripts/rc-acceptance/measurement.ps1 scripts/tests/rc-acceptance-measurement.Tests.ps1
git commit -m "feat: measure bounded RC process trees"
```

### Task 4: 隔离 RC 验收入口与端到端报告

**Files:**
- Create: `scripts/rc-acceptance.ps1`
- Create: `scripts/tests/rc-acceptance.Tests.ps1`
- Modify: `package.json`

**Interfaces:**
- Consumes: `-Installer`, `-AuditDirectory`、可选 `-DesktopProcessId` 和 `-ObservationSeconds`。
- Produces: 全新目录中的 `evidence.json` 和 `report.md`，进程采样省略时保持 `not_run`。

- [ ] **Step 1: 编写入口参数和目录边界失败测试**

新增 helper，以独立 PowerShell 进程执行入口并捕获退出码。覆盖：

- 缺少 Installer/AuditDirectory；
- 非 `.exe` 或不存在安装包；
- AuditDirectory 已存在；
- AuditDirectory 是盘符根、仓库根、当前用户根；
- AuditDirectory 的现有父级是 reparse point；
- ObservationSeconds 为 0、4、301；
- PID 为 0 或不存在；
- 无 PID 时成功生成 `process_observation.status = not_run`。

所有失败断言只匹配固定码：`installer_invalid`、`audit_directory_invalid`、
`audit_directory_exists`、`observation_seconds_invalid`、`desktop_process_missing`。

- [ ] **Step 2: 运行入口测试并确认脚本缺失**

Run:

```powershell
pwsh -NoProfile -File scripts/tests/rc-acceptance.Tests.ps1
```

Expected: FAIL，指出入口不存在。

- [ ] **Step 3: 实现先校验、后创建、只写新目录的入口**

入口声明：

```powershell
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $Installer,
    [Parameter(Mandatory)][string] $AuditDirectory,
    [ValidateRange(1, [int]::MaxValue)][int] $DesktopProcessId,
    [ValidateRange(5, 300)][int] $ObservationSeconds = 60
)
```

处理顺序必须是：

1. 验证 PowerShell 7 和 Windows x64；
2. 解析仓库根和安装包普通文件；
3. 使用 `Path.GetFullPath` 解析审计目录，拒绝盘符根、仓库根、用户根、已存在目标；
4. 逐级检查所有已存在父目录的 reparse 属性；
5. 如提供 PID，先验证首次进程 sample；
6. `New-Item -ItemType Directory` 创建唯一审计目录；
7. 采集 build/environment/installer/process 对象；
8. 生成默认人工 checks 和 overall result；
9. 用 `Write-RcEvidenceFiles` 排他写入两个文件；
10. 只输出审计目录和固定完成提示，不输出动态错误正文。

Git commit 使用：

```powershell
$gitCommit = (& git -C $repositoryRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $gitCommit -cnotmatch '^[0-9a-f]{40}$') {
    throw 'git_commit_unavailable'
}
```

入口顶层 catch 把批准的固定异常原样写到 error stream；其他异常统一为
`rc_acceptance_failed`。

- [ ] **Step 4: 增加成功端到端测试和 schema 白名单检查**

用测试 `fixture.exe`、全新审计路径和无 PID 模式运行入口。解析 `evidence.json`，断言：

- `schema_version = 1`；
- 顶层属性恰好为 spec 定义的七项；
- `result = not_run`；
- `process_observation.status = not_run`；
- 每个人工检查初始为 `not_run`；
- JSON/Markdown 不含 fixture 绝对目录、用户名、`CommandLine`、`API_KEY`；
- 再次使用相同审计目录失败且原 bytes 不变。

- [ ] **Step 5: 把快速报告/入口测试接入标准门禁**

在 `package.json` 增加：

```json
"test:rc-acceptance": "pwsh -NoProfile -File scripts/tests/rc-acceptance-report.Tests.ps1 && pwsh -NoProfile -File scripts/tests/rc-acceptance-measurement.Tests.ps1 && pwsh -NoProfile -File scripts/tests/rc-acceptance.Tests.ps1"
```

并在 `check` 的 `pnpm test` 之后调用 `pnpm test:rc-acceptance`。不把真实 60 秒采样或 GUI
操作放入标准门禁；测试使用计划中固定的 5 秒受控进程。

- [ ] **Step 6: 运行新入口测试和完整门禁**

Run:

```powershell
pnpm test:rc-acceptance
git diff --check
pnpm check
```

Expected: 全部 PASS。

- [ ] **Step 7: 提交入口与门禁**

```powershell
git add scripts/rc-acceptance.ps1 scripts/tests/rc-acceptance.Tests.ps1 package.json
git commit -m "feat: add isolated Windows RC acceptance entrypoint"
```

### Task 5: 开发文档与真实 RC 仅采集验证

**Files:**
- Modify: `docs/development.md`
- Test: `scripts/tests/rc-acceptance.Tests.ps1`

**Interfaces:**
- Consumes: Task 4 的命令接口和证据文件。
- Produces: 发布维护者可重复执行的自动采集、人工验收和外部条件说明。

- [ ] **Step 1: 编写文档契约失败测试**

在入口测试末尾读取 `docs/development.md`，断言包含：

```text
scripts/rc-acceptance.ps1
-Installer
-AuditDirectory
-DesktopProcessId
evidence.json
report.md
not_run
不得自动断网
不得自动终止
Authenticode
Windows 10
```

Expected: 当前文档至少缺少新入口而失败。

- [ ] **Step 2: 更新开发文档的标准 RC 顺序**

在“Windows RC 手工验收矩阵”之前增加“RC 自动证据采集”小节：

1. 创建专用当前用户测试安装和全新测试数据；
2. 对安装包运行无 PID 预检，记录 SHA-256 与签名状态；
3. 人工启动 RC，确认 PID 后执行默认视觉 60 秒采样；
4. 启用 8K 测试皮肤后对同一版本执行另一个全新目录的 60 秒采样；
5. 人工执行离线、托盘、通知、回滚、皮肤和缩放矩阵；
6. Windows 10、WebView2 缺失补装、Authenticode 缺证书时保持 `not_run`；
7. 最后运行 `git diff --check` 与 `pnpm check`。

明确脚本不得自动断网、不得自动终止进程、不得访问真实用户数据，审计目录不会自动删除。

- [ ] **Step 3: 运行文档契约和完整门禁**

Run:

```powershell
pnpm test:rc-acceptance
git diff --check
pnpm check
```

Expected: 全部 PASS。

- [ ] **Step 4: 查找真实 RC 安装包并执行只读采集**

Run:

```powershell
$installer = Get-ChildItem -LiteralPath src-tauri/target/release/bundle/nsis `
  -Filter '*.exe' -File -ErrorAction SilentlyContinue |
  Sort-Object LastWriteTimeUtc -Descending |
  Select-Object -First 1
if ($null -ne $installer) {
    $auditRoot = Join-Path ([System.IO.Path]::GetTempPath()) `
      "dsh-desktop-rc-acceptance-$([guid]::NewGuid().ToString('N'))"
    pwsh -NoProfile -File scripts/rc-acceptance.ps1 `
      -Installer $installer.FullName `
      -AuditDirectory $auditRoot
} else {
    Write-Output 'real_rc_installer_not_available: 保持 not_run，不临时构建或伪造制品'
}
```

Expected: 有安装包时生成 JSON/Markdown 且整体为 `not_run`；无安装包时明确记录外部条件，
不得把开发 exe 或假夹具当作真实 RC。

- [ ] **Step 5: 最终自检与提交**

Run:

```powershell
rg -n "Remove-Item|\.Delete\(|\.Kill\(|Stop-Process|CommandLine|EnvironmentVariables" `
  scripts/rc-acceptance.ps1 scripts/rc-acceptance scripts/tests/rc-acceptance*.Tests.ps1
git diff --check
pnpm check
git status --short
```

Expected: 产品/测试脚本不包含删除、强制终止、命令行或环境变量采集；所有门禁通过；只剩
本任务预期修改。

Commit:

```powershell
git add docs/development.md scripts/tests/rc-acceptance.Tests.ps1
git commit -m "docs: document repeatable Windows RC acceptance"
```

## Plan Self-Review

- Spec coverage: 安装包、版本、Windows/WebView2、指定 PID、进程组性能、JSON/Markdown、
  人工 `not_run`、隔离边界、无删除和开发文档分别由 Tasks 1–5 覆盖。
- Scope: 不自动安装/卸载、不改网络、不强制终止进程、不配置证书、不宣称跨 Windows 版本
  通过；真实产品缺陷保持独立 TDD 修复。
- Type consistency: Tasks 1–5 统一使用 `New-RcCheckList`、`Get-RcOverallResult`、
  `Get-RcBuildEvidence`、`Get-RcEnvironmentEvidence`、`Get-RcInstallerEvidence`、
  `Measure-RcProcessTree` 和 `Write-RcEvidenceFiles`。
- Placeholder scan: 计划不包含占位语句、跨任务省略或未定义的后续接口。
