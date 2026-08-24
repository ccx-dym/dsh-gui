$ErrorActionPreference = 'Stop'

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
. (Join-Path $repositoryRoot 'scripts\rc-acceptance\report.ps1')

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

# 缺少或改名人工检查项会让 RC 报告无法与固定验收矩阵对应。
$checks = @(New-RcCheckList)
$expectedIds = @(
    'first_install',
    'offline_restart',
    'notification_split',
    'busy_rejection',
    'tray_lifecycle',
    'existing_pair_rollback',
    'complete_exit',
    'skin_import',
    'skin_layout',
    'skin_persistence',
    'adapter_fallback',
    'scale_layout',
    'default_performance',
    'skin_8k_performance',
    'windows_10',
    'webview2_bootstrapper',
    'authenticode'
)
Assert-Equal -Actual ($checks.id -join ',') -Expected ($expectedIds -join ',') `
    -Message '人工检查 ID 必须保持稳定'
Assert-Equal -Actual @($checks | Where-Object status -cne 'not_run').Count -Expected 0 `
    -Message '人工检查必须默认未执行'

# 状态收敛必须失败优先，且任何未执行项都不能得到整体通过。
Assert-Equal -Actual (Get-RcOverallResult -Checks $checks) -Expected 'not_run' `
    -Message '未执行状态必须向顶层传播'
$passed = @([ordered]@{ id = 'a'; status = 'passed'; evidence = '固定证据' })
Assert-Equal -Actual (Get-RcOverallResult -Checks $passed) -Expected 'passed' `
    -Message '全部通过时整体应通过'
$failed = @(
    [ordered]@{ id = 'a'; status = 'passed'; evidence = '固定证据' },
    [ordered]@{ id = 'b'; status = 'failed'; evidence = '固定证据' },
    [ordered]@{ id = 'c'; status = 'not_run'; evidence = '' }
)
Assert-Equal -Actual (Get-RcOverallResult -Checks $failed) -Expected 'failed' `
    -Message '失败必须优先于未执行'

try {
    Get-RcOverallResult -Checks @([ordered]@{ id = 'a'; status = 'unknown'; evidence = '' })
    throw '非法状态不应被接受'
} catch {
    Assert-Equal -Actual $_.Exception.Message -Expected 'rc_status_invalid' `
        -Message '非法状态必须映射固定错误'
}

# 报告只渲染批准字段；绝对路径、命令行和敏感环境变量即使混入对象也不得输出。
$auditRoot = Join-Path ([System.IO.Path]::GetTempPath()) `
    "dsh-rc-report-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $auditRoot | Out-Null
$evidence = [ordered]@{
    schema_version = 1
    generated_at_utc = '2026-08-24T00:00:00Z'
    result = 'not_run'
    build = [ordered]@{
        desktop_version = '0.1.13'
        git_commit = ('a' * 40)
        CommandLine = '--secret'
    }
    environment = [ordered]@{
        windows_caption = 'Windows test'
        windows_version = '10.0'
        windows_build = '12345'
        logical_processors = 8
        physical_memory_bytes = 1024
        webview2_version = '1.2.3.4'
        API_KEY = 'secret'
    }
    installer = [ordered]@{
        file_name = 'fixture.exe'
        size_bytes = 4
        sha256 = ('0' * 64)
        authenticode = 'NotSigned'
        path = (Join-Path $auditRoot 'fixture.exe')
    }
    process_observation = [ordered]@{
        status = 'passed'
        desktop_process_id = 1234
        observation_seconds = 60
        elapsed_seconds = 60.1
        root_process = [ordered]@{
            process_count = 1
            cpu_percent = 0.1
            working_set_bytes = 100
            private_bytes = 80
            CommandLine = '--nested-secret'
        }
        descendants = [ordered]@{ process_count = 0; cpu_percent = $null; working_set_bytes = 0; private_bytes = 0 }
        webview2 = [ordered]@{ process_count = 0; cpu_percent = $null; working_set_bytes = 0; private_bytes = 0 }
        node = [ordered]@{ process_count = 0; cpu_percent = $null; working_set_bytes = 0; private_bytes = 0 }
        new_process_ids = @()
        exited_process_ids = @()
    }
    checks = $checks
}

Write-RcEvidenceFiles -Evidence $evidence -AuditDirectory $auditRoot
$jsonPath = Join-Path $auditRoot 'evidence.json'
$markdownPath = Join-Path $auditRoot 'report.md'
$before = [System.IO.File]::ReadAllBytes($jsonPath)
$json = [System.IO.File]::ReadAllText($jsonPath)
$markdown = [System.IO.File]::ReadAllText($markdownPath)

if ($markdown -notmatch '(?s)offline_restart.*not_run') {
    throw 'Markdown 必须显示固定人工检查及其未执行状态'
}
foreach ($content in @($json, $markdown)) {
    if ($content -match [regex]::Escape($auditRoot) -or
        $content -match 'CommandLine|API_KEY|--secret|--nested-secret') {
        throw '证据文件包含未批准的敏感字段'
    }
}

try {
    Write-RcEvidenceFiles -Evidence $evidence -AuditDirectory $auditRoot
    throw '重复写入不应成功'
} catch {
    Assert-Equal -Actual $_.Exception.Message -Expected 'rc_evidence_exists' `
        -Message '重复写入必须返回固定错误'
}
$after = [System.IO.File]::ReadAllBytes($jsonPath)
if (-not [System.Linq.Enumerable]::SequenceEqual[byte]($before, $after)) {
    throw '重复写入不得改变已有证据'
}

Write-Output "RC acceptance report tests passed; audit directory: $auditRoot"
