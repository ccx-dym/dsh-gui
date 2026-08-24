[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $Installer,
    [Parameter(Mandatory)][string] $AuditDirectory,
    [int] $DesktopProcessId,
    [int] $ObservationSeconds = 60
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$fixedErrors = @(
    'installer_invalid',
    'installer_read_failed',
    'audit_directory_invalid',
    'audit_directory_exists',
    'observation_seconds_invalid',
    'desktop_process_missing',
    'process_observation_unavailable',
    'desktop_version_invalid',
    'desktop_version_mismatch',
    'environment_unavailable',
    'git_commit_unavailable',
    'rc_evidence_exists',
    'rc_status_invalid',
    'platform_unsupported'
)

function Test-RcSamePath {
    param(
        [Parameter(Mandatory)][string] $Left,
        [Parameter(Mandatory)][string] $Right
    )

    return [string]::Equals(
        $Left.TrimEnd('\', '/'),
        $Right.TrimEnd('\', '/'),
        [System.StringComparison]::OrdinalIgnoreCase
    )
}

function Resolve-RcAuditDirectory {
    param(
        [Parameter(Mandatory)][string] $RequestedPath,
        [Parameter(Mandatory)][string] $RepositoryRoot
    )

    try {
        $fullPath = [System.IO.Path]::GetFullPath($RequestedPath)
        $pathRoot = [System.IO.Path]::GetPathRoot($fullPath)
        $userRoot = [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
    } catch {
        throw 'audit_directory_invalid'
    }
    foreach ($unsafePath in @($pathRoot, $RepositoryRoot, $userRoot)) {
        if (-not [string]::IsNullOrWhiteSpace($unsafePath) -and
            (Test-RcSamePath -Left $fullPath -Right $unsafePath)) {
            throw 'audit_directory_invalid'
        }
    }
    if (Test-Path -LiteralPath $fullPath) {
        throw 'audit_directory_exists'
    }

    # 检查所有现存祖先，避免新目录实际落入 junction 或符号链接的外部目标。
    $ancestor = [System.IO.DirectoryInfo]::new([System.IO.Path]::GetDirectoryName($fullPath))
    $foundExistingParent = $false
    while ($null -ne $ancestor) {
        if ($ancestor.Exists) {
            $foundExistingParent = $true
            if ($ancestor.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
                throw 'audit_directory_invalid'
            }
        }
        $ancestor = $ancestor.Parent
    }
    if (-not $foundExistingParent) {
        throw 'audit_directory_invalid'
    }
    return $fullPath
}

try {
    if (-not $IsWindows -or -not [Environment]::Is64BitOperatingSystem) {
        throw 'platform_unsupported'
    }
    if ($ObservationSeconds -lt 5 -or $ObservationSeconds -gt 300) {
        throw 'observation_seconds_invalid'
    }
    if ($PSBoundParameters.ContainsKey('DesktopProcessId') -and $DesktopProcessId -lt 1) {
        throw 'desktop_process_missing'
    }

    $repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
    . (Join-Path $PSScriptRoot 'rc-acceptance\measurement.ps1')
    . (Join-Path $PSScriptRoot 'rc-acceptance\report.ps1')

    # 安装包和可选 PID 必须在创建审计目录前通过，避免无效输入留下假报告目录。
    $installerEvidence = Get-RcInstallerEvidence -Installer $Installer
    $resolvedAudit = Resolve-RcAuditDirectory -RequestedPath $AuditDirectory `
        -RepositoryRoot $repositoryRoot
    $initialProcessSample = $null
    if ($PSBoundParameters.ContainsKey('DesktopProcessId')) {
        $initialProcessSample = Get-RcProcessSample -DesktopProcessId $DesktopProcessId
    }

    $gitCommit = (& git -C $repositoryRoot rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or $gitCommit -cnotmatch '^[0-9a-f]{40}$') {
        throw 'git_commit_unavailable'
    }
    $buildEvidence = Get-RcBuildEvidence -RepositoryRoot $repositoryRoot `
        -GitCommit $gitCommit
    $environmentEvidence = Get-RcEnvironmentEvidence

    New-Item -ItemType Directory -Path $resolvedAudit | Out-Null

    $processObservation = [ordered]@{ status = 'not_run' }
    if ($null -ne $initialProcessSample) {
        # Measure 会重新读取首次样本，保证正式采样窗口从报告阶段开始且 PID 仍然存活。
        $processObservation = Measure-RcProcessTree -DesktopProcessId $DesktopProcessId `
            -ObservationSeconds $ObservationSeconds
    }
    $checks = @(New-RcCheckList)
    $evidence = [ordered]@{
        schema_version = 1
        generated_at_utc = [DateTimeOffset]::UtcNow.ToString(
            'yyyy-MM-ddTHH:mm:ssZ',
            [Globalization.CultureInfo]::InvariantCulture
        )
        result = Get-RcOverallResult -Checks $checks
        build = $buildEvidence
        environment = $environmentEvidence
        installer = $installerEvidence
        process_observation = $processObservation
        checks = $checks
    }
    Write-RcEvidenceFiles -Evidence $evidence -AuditDirectory $resolvedAudit
    Write-Output "RC 验收证据已生成；审计目录: $resolvedAudit"
} catch {
    $message = $_.Exception.Message
    if ($message -cnotin $fixedErrors) {
        $message = 'rc_acceptance_failed'
    }
    [Console]::Error.WriteLine($message)
    exit 1
}
