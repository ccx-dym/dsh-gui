$ErrorActionPreference = 'Stop'

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
. (Join-Path $repositoryRoot 'scripts\rc-acceptance\measurement.ps1')

function Assert-Equal {
    param(
        [Parameter(Mandatory)] $Actual,
        [Parameter(Mandatory)] $Expected,
        [Parameter(Mandatory)][string] $Message
    )

    if ($Actual -cne $Expected) {
        throw "$Message：expected=$Expected actual=$Actual"
    }
}

function Assert-FixedFailure {
    param(
        [Parameter(Mandatory)][scriptblock] $Action,
        [Parameter(Mandatory)][string] $Expected
    )

    try {
        & $Action
        throw '预期操作失败'
    } catch {
        Assert-Equal -Actual $_.Exception.Message -Expected $Expected `
            -Message '失败必须使用固定错误码'
    }
}

# 独立夹具验证三处版本的真实解析，不读取或修改仓库配置。
$fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) `
    "dsh-rc-measure-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path (Join-Path $fixtureRoot 'src-tauri') | Out-Null
[System.IO.File]::WriteAllText(
    (Join-Path $fixtureRoot 'package.json'),
    '{"version":"0.1.13"}'
)
[System.IO.File]::WriteAllText(
    (Join-Path $fixtureRoot 'src-tauri\Cargo.toml'),
    "[package]`nname = `"fixture`"`nversion = `"0.1.13`"`n`n[dependencies]`n"
)
[System.IO.File]::WriteAllText(
    (Join-Path $fixtureRoot 'src-tauri\tauri.conf.json'),
    '{"version":"0.1.13"}'
)

$build = Get-RcBuildEvidence -RepositoryRoot $fixtureRoot -GitCommit ('a' * 40)
Assert-Equal -Actual $build.desktop_version -Expected '0.1.13' `
    -Message '桌面版本应来自三处一致配置'
Assert-Equal -Actual $build.git_commit -Expected ('a' * 40) `
    -Message 'Git commit 应保持规范小写 SHA'

# 摘要使用手工验证的四字节夹具，防止测试复用生产摘要算法形成镜像断言。
$installer = Join-Path $fixtureRoot 'fixture.exe'
[System.IO.File]::WriteAllBytes($installer, [byte[]](0, 1, 2, 3))
$artifact = Get-RcInstallerEvidence -Installer $installer
Assert-Equal -Actual $artifact.file_name -Expected 'fixture.exe' `
    -Message '报告只应包含安装包文件名'
Assert-Equal -Actual $artifact.size_bytes -Expected 4 `
    -Message '安装包字节数应来自锁定文件'
Assert-Equal -Actual $artifact.sha256 `
    -Expected '054edec1d0211f624fed0cbca9d4f9400b0e491c43742af2c5b0abebf0c990d8' `
    -Message '安装包 SHA-256 应匹配实际 bytes'
Assert-Equal -Actual $artifact.authenticode -Expected 'UnknownError' `
    -Message '非 PE 的 exe 夹具应映射为固定签名错误'
if ($artifact.PSObject.Properties.Name -contains 'path') {
    throw '安装包绝对路径不得离开采集边界'
}

# 版本漂移必须失败关闭，不能选择其中任一版本继续生成报告。
[System.IO.File]::WriteAllText(
    (Join-Path $fixtureRoot 'src-tauri\Cargo.toml'),
    "[package]`nname = `"fixture`"`nversion = `"0.1.12`"`n"
)
Assert-FixedFailure -Expected 'desktop_version_mismatch' -Action {
    Get-RcBuildEvidence -RepositoryRoot $fixtureRoot -GitCommit ('a' * 40)
}

$notInstaller = Join-Path $fixtureRoot 'fixture.zip'
[System.IO.File]::WriteAllBytes($notInstaller, [byte[]](0, 1, 2, 3))
Assert-FixedFailure -Expected 'installer_invalid' -Action {
    Get-RcInstallerEvidence -Installer $notInstaller
}
Assert-FixedFailure -Expected 'installer_invalid' -Action {
    Get-RcInstallerEvidence -Installer $fixtureRoot
}

# 系统证据只允许非身份字段；机器名、用户名、SID 和路径不属于验收指标。
$environment = Get-RcEnvironmentEvidence
$expectedEnvironmentFields = @(
    'windows_caption',
    'windows_version',
    'windows_build',
    'logical_processors',
    'physical_memory_bytes',
    'webview2_version'
)
Assert-Equal -Actual ($environment.PSObject.Properties.Name -join ',') `
    -Expected ($expectedEnvironmentFields -join ',') `
    -Message '环境证据必须保持固定白名单'
if ($environment.logical_processors -lt 1 -or $environment.physical_memory_bytes -lt 1) {
    throw '环境数值必须来自有效系统信息'
}

# 进程树只允许根 PID 及递归后代；同名旁系进程不能因为名称相同被纳入。
$rows = @(
    [pscustomobject]@{ ProcessId = 10; ParentProcessId = 1; Name = 'dsh-desktop.exe' },
    [pscustomobject]@{ ProcessId = 11; ParentProcessId = 10; Name = 'msedgewebview2.exe' },
    [pscustomobject]@{ ProcessId = 12; ParentProcessId = 11; Name = 'node.exe' },
    [pscustomobject]@{ ProcessId = 20; ParentProcessId = 1; Name = 'node.exe' }
)
$tree = @(Select-RcProcessTree -Rows $rows -RootProcessId 10)
Assert-Equal -Actual ($tree.ProcessId -join ',') -Expected '10,11,12' `
    -Message '进程树必须限定到指定根节点'
Assert-FixedFailure -Expected 'desktop_process_missing' -Action {
    Select-RcProcessTree -Rows $rows -RootProcessId 999
}

# 前后样本使用手工计算的 CPU 和内存期望，能捕获逻辑处理器除数、退出/新增分类错误。
$before = [pscustomobject]@{
    root_process_id = 10
    processes = @(
        [pscustomobject]@{ process_id = 10; parent_process_id = 1; process_name = 'dsh-desktop'; total_processor_time_ms = 1000.0; working_set_bytes = 400; private_bytes = 300 },
        [pscustomobject]@{ process_id = 11; parent_process_id = 10; process_name = 'msedgewebview2'; total_processor_time_ms = 200.0; working_set_bytes = 200; private_bytes = 150 },
        [pscustomobject]@{ process_id = 12; parent_process_id = 11; process_name = 'node'; total_processor_time_ms = 100.0; working_set_bytes = 80; private_bytes = 60 }
    )
}
$after = [pscustomobject]@{
    root_process_id = 10
    processes = @(
        [pscustomobject]@{ process_id = 10; parent_process_id = 1; process_name = 'dsh-desktop'; total_processor_time_ms = 1400.0; working_set_bytes = 500; private_bytes = 350 },
        [pscustomobject]@{ process_id = 11; parent_process_id = 10; process_name = 'msedgewebview2'; total_processor_time_ms = 400.0; working_set_bytes = 300; private_bytes = 250 },
        [pscustomobject]@{ process_id = 13; parent_process_id = 10; process_name = 'node'; total_processor_time_ms = 50.0; working_set_bytes = 100; private_bytes = 70 }
    )
}
$comparison = Compare-RcProcessSamples -Before $before -After $after `
    -ElapsedSeconds 2 -LogicalProcessors 2 -ObservationSeconds 2
Assert-Equal -Actual $comparison.root_process.cpu_percent -Expected 10.0 `
    -Message '根进程 CPU 应按采样时间和逻辑处理器数归一化'
Assert-Equal -Actual $comparison.descendants.cpu_percent -Expected 5.0 `
    -Message '后代 CPU 只应统计前后均存在的进程'
Assert-Equal -Actual $comparison.descendants.working_set_bytes -Expected 400 `
    -Message '后代内存应统计采样结束时存活进程'
Assert-Equal -Actual $comparison.webview2.process_count -Expected 1 `
    -Message 'WebView2 聚合应按固定进程名分类'
Assert-Equal -Actual $comparison.node.process_count -Expected 1 `
    -Message 'Node 聚合应包含采样中新出现的存活进程'
Assert-Equal -Actual ($comparison.new_process_ids -join ',') -Expected '13' `
    -Message '新增 PID 必须单独记录'
Assert-Equal -Actual ($comparison.exited_process_ids -join ',') -Expected '12' `
    -Message '退出 PID 必须单独记录'

# 真实集成采样只观察一个固定寿命的隐藏测试进程，不读取命令行且不主动终止。
$testProcess = Start-Process -FilePath (Get-Command pwsh).Source `
    -ArgumentList @('-NoProfile', '-Command', 'Start-Sleep -Seconds 8') `
    -PassThru -WindowStyle Hidden
$observation = Measure-RcProcessTree -DesktopProcessId $testProcess.Id -ObservationSeconds 5
Assert-Equal -Actual $observation.status -Expected 'passed' `
    -Message '存在的指定 PID 应产生已执行证据'
Assert-Equal -Actual $observation.desktop_process_id -Expected $testProcess.Id `
    -Message '采样必须绑定调用者提供的 PID'
$serializedObservation = $observation | ConvertTo-Json -Depth 10
if ($serializedObservation -match 'CommandLine|Path|Environment|WindowTitle') {
    throw '进程证据包含未批准的敏感字段'
}

Write-Output "RC acceptance measurement tests passed; fixture directory: $fixtureRoot"
