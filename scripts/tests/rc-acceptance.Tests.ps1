$ErrorActionPreference = 'Stop'

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$scriptPath = Join-Path $repositoryRoot 'scripts\rc-acceptance.ps1'

function Invoke-RcAcceptanceTest {
    param([Parameter(Mandatory)][string[]] $Arguments)

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = (Get-Command pwsh).Source
    $startInfo.ArgumentList.Add('-NoProfile')
    $startInfo.ArgumentList.Add('-File')
    $startInfo.ArgumentList.Add($scriptPath)
    foreach ($argument in $Arguments) {
        $startInfo.ArgumentList.Add($argument)
    }
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.UseShellExecute = $false
    $process = [System.Diagnostics.Process]::Start($startInfo)
    $stdout = $process.StandardOutput.ReadToEnd()
    $stderr = $process.StandardError.ReadToEnd()
    $process.WaitForExit()
    return [pscustomobject]@{
        ExitCode = $process.ExitCode
        Output = "$stdout`n$stderr"
    }
}

function Assert-FailsWith {
    param(
        [Parameter(Mandatory)][string[]] $Arguments,
        [Parameter(Mandatory)][string] $Code
    )

    $result = Invoke-RcAcceptanceTest -Arguments $Arguments
    if ($result.ExitCode -eq 0 -or $result.Output -cnotmatch "(?m)^$([regex]::Escape($Code))\s*$") {
        throw "预期固定失败 $Code，实际 exit=$($result.ExitCode)，output=$($result.Output)"
    }
}

$fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) `
    "dsh-rc-entry-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
$installer = Join-Path $fixtureRoot 'fixture.exe'
[System.IO.File]::WriteAllBytes($installer, [byte[]](0, 1, 2, 3))
$notInstaller = Join-Path $fixtureRoot 'fixture.zip'
[System.IO.File]::WriteAllBytes($notInstaller, [byte[]](0, 1, 2, 3))

# 参数和路径边界必须在创建审计目录之前失败。
Assert-FailsWith -Code 'installer_invalid' -Arguments @(
    '-Installer', $notInstaller,
    '-AuditDirectory', (Join-Path $fixtureRoot 'bad-installer-audit')
)
Assert-FailsWith -Code 'installer_invalid' -Arguments @(
    '-Installer', (Join-Path $fixtureRoot 'missing.exe'),
    '-AuditDirectory', (Join-Path $fixtureRoot 'missing-installer-audit')
)

$existingAudit = Join-Path $fixtureRoot 'existing-audit'
New-Item -ItemType Directory -Path $existingAudit | Out-Null
Assert-FailsWith -Code 'audit_directory_exists' -Arguments @(
    '-Installer', $installer,
    '-AuditDirectory', $existingAudit
)
foreach ($unsafeRoot in @(
    [System.IO.Path]::GetPathRoot($repositoryRoot),
    $repositoryRoot,
    [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
)) {
    Assert-FailsWith -Code 'audit_directory_invalid' -Arguments @(
        '-Installer', $installer,
        '-AuditDirectory', $unsafeRoot
    )
}

# 审计目录不能经符号链接或 junction 落到调用者未明确指定的真实位置。
$realParent = Join-Path $fixtureRoot 'real-parent'
$linkedParent = Join-Path $fixtureRoot 'linked-parent'
New-Item -ItemType Directory -Path $realParent | Out-Null
try {
    New-Item -ItemType Junction -Path $linkedParent -Target $realParent `
        -ErrorAction Stop | Out-Null
    Assert-FailsWith -Code 'audit_directory_invalid' -Arguments @(
        '-Installer', $installer,
        '-AuditDirectory', (Join-Path $linkedParent 'linked-audit')
    )
} catch [System.UnauthorizedAccessException] {
    Write-Output 'junction fixture unavailable; reparse parent case not run'
}
Assert-FailsWith -Code 'observation_seconds_invalid' -Arguments @(
    '-Installer', $installer,
    '-AuditDirectory', (Join-Path $fixtureRoot 'short-observation'),
    '-ObservationSeconds', '4'
)
Assert-FailsWith -Code 'observation_seconds_invalid' -Arguments @(
    '-Installer', $installer,
    '-AuditDirectory', (Join-Path $fixtureRoot 'long-observation'),
    '-ObservationSeconds', '301'
)
Assert-FailsWith -Code 'desktop_process_missing' -Arguments @(
    '-Installer', $installer,
    '-AuditDirectory', (Join-Path $fixtureRoot 'missing-process'),
    '-DesktopProcessId', '2147483647',
    '-ObservationSeconds', '5'
)

# 无 PID 模式必须生成真实元数据，但所有进程和人工检查保持 not_run。
$auditRoot = Join-Path $fixtureRoot 'successful-audit'
$result = Invoke-RcAcceptanceTest -Arguments @(
    '-Installer', $installer,
    '-AuditDirectory', $auditRoot
)
if ($result.ExitCode -ne 0) {
    throw "无 PID 验收应成功，实际 output=$($result.Output)"
}
$jsonPath = Join-Path $auditRoot 'evidence.json'
$markdownPath = Join-Path $auditRoot 'report.md'
$before = [System.IO.File]::ReadAllBytes($jsonPath)
$evidence = Get-Content -Raw -LiteralPath $jsonPath | ConvertFrom-Json
$expectedFields = @(
    'schema_version', 'generated_at_utc', 'result', 'build', 'environment',
    'installer', 'process_observation', 'checks'
)
if (($evidence.PSObject.Properties.Name -join ',') -cne ($expectedFields -join ',')) {
    throw 'evidence 顶层 schema 不匹配'
}
if ($evidence.schema_version -ne 1 -or $evidence.result -cne 'not_run' -or
    $evidence.process_observation.status -cne 'not_run') {
    throw '无 PID 验收不得伪造整体或进程通过状态'
}
if (@($evidence.checks | Where-Object status -cne 'not_run').Count -ne 0) {
    throw '首次报告的人工检查必须全部未执行'
}
foreach ($content in @(
    [System.IO.File]::ReadAllText($jsonPath),
    [System.IO.File]::ReadAllText($markdownPath)
)) {
    if ($content -match [regex]::Escape($fixtureRoot) -or
        $content -match 'CommandLine|API_KEY|EnvironmentVariables') {
        throw '入口报告包含路径或未批准敏感字段'
    }
}

# 相同目录再次执行必须失败，且不能覆盖首次证据。
Assert-FailsWith -Code 'audit_directory_exists' -Arguments @(
    '-Installer', $installer,
    '-AuditDirectory', $auditRoot
)
$after = [System.IO.File]::ReadAllBytes($jsonPath)
if (-not [System.Linq.Enumerable]::SequenceEqual[byte]($before, $after)) {
    throw '重复入口执行改变了已有证据'
}

Write-Output "RC acceptance entrypoint tests passed; fixture directory: $fixtureRoot"
