Set-StrictMode -Version Latest

$script:ExactDesktopSemver = `
    '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*)?$'
$script:MaximumConfigBytes = 1MB

function Read-RcBoundedText {
    param([Parameter(Mandatory)][string] $Path)

    $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
    if ($null -eq $item -or $item.PSIsContainer -or $item.Length -gt $script:MaximumConfigBytes -or
        ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
        throw 'desktop_version_invalid'
    }
    return [System.IO.File]::ReadAllText($item.FullName, [System.Text.Encoding]::UTF8)
}

function Get-RcCargoPackageVersion {
    param([Parameter(Mandatory)][string] $Source)

    $insidePackage = $false
    $versions = [System.Collections.Generic.List[string]]::new()
    foreach ($line in ($Source -split "`r?`n")) {
        if ($line -match '^\s*\[(?<section>[^]]+)\]\s*$') {
            $insidePackage = $Matches.section -ceq 'package'
            continue
        }
        if ($insidePackage -and $line -match '^\s*version\s*=\s*"(?<version>[^"]+)"\s*$') {
            $versions.Add($Matches.version)
        }
    }
    if ($versions.Count -ne 1) {
        throw 'desktop_version_invalid'
    }
    return $versions[0]
}

function Get-RcBuildEvidence {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string] $RepositoryRoot,
        [Parameter(Mandatory)][ValidatePattern('^[0-9a-f]{40}$')][string] $GitCommit
    )

    try {
        $package = Read-RcBoundedText -Path (Join-Path $RepositoryRoot 'package.json') |
            ConvertFrom-Json -ErrorAction Stop
        $tauri = Read-RcBoundedText -Path `
            (Join-Path $RepositoryRoot 'src-tauri\tauri.conf.json') |
            ConvertFrom-Json -ErrorAction Stop
        $cargoSource = Read-RcBoundedText -Path `
            (Join-Path $RepositoryRoot 'src-tauri\Cargo.toml')
        $cargoVersion = Get-RcCargoPackageVersion -Source $cargoSource
        $versions = @([string]$package.version, [string]$tauri.version, $cargoVersion)
    } catch {
        if ($_.Exception.Message -ceq 'desktop_version_invalid') {
            throw
        }
        throw 'desktop_version_invalid'
    }

    if (@($versions | Where-Object { $_ -cnotmatch $script:ExactDesktopSemver }).Count -gt 0 -or
        @($versions | Select-Object -Unique).Count -ne 1) {
        throw 'desktop_version_mismatch'
    }
    return [pscustomobject][ordered]@{
        desktop_version = $versions[0]
        git_commit = $GitCommit
    }
}

function Get-RcAuthenticodeCategory {
    param([Parameter(Mandatory)] $Signature)

    switch ([string]$Signature.Status) {
        'Valid' { 'Valid' }
        'NotSigned' { 'NotSigned' }
        'HashMismatch' { 'HashMismatch' }
        'NotTrusted' { 'NotTrusted' }
        default { 'UnknownError' }
    }
}

function Get-RcInstallerEvidence {
    [CmdletBinding()]
    param([Parameter(Mandatory)][string] $Installer)

    $item = Get-Item -LiteralPath $Installer -Force -ErrorAction SilentlyContinue
    if ($null -eq $item -or $item.PSIsContainer -or $item.Extension -cne '.exe' -or
        ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
        throw 'installer_invalid'
    }

    # 共享读锁阻止验收期间的覆盖写入；摘要和签名必须绑定同一稳定文件。
    try {
        $stream = [System.IO.FileStream]::new(
            $item.FullName,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read,
            [System.IO.FileShare]::Read
        )
        try {
            $length = [uint64]$stream.Length
            $digestBytes = [System.Security.Cryptography.SHA256]::HashData($stream)
            $sha256 = [Convert]::ToHexString($digestBytes).ToLowerInvariant()
            $signature = Get-AuthenticodeSignature -FilePath $item.FullName
            $authenticode = Get-RcAuthenticodeCategory -Signature $signature
        } finally {
            $stream.Dispose()
        }
    } catch {
        if ($_.Exception.Message -ceq 'installer_invalid') {
            throw
        }
        throw 'installer_read_failed'
    }

    return [pscustomobject][ordered]@{
        file_name = $item.Name
        size_bytes = $length
        sha256 = $sha256
        authenticode = $authenticode
    }
}

function Get-RcWebView2Version {
    $clientId = '{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    $keys = @(
        "HKCU:\Software\Microsoft\EdgeUpdate\Clients\$clientId",
        "HKLM:\Software\WOW6432Node\Microsoft\EdgeUpdate\Clients\$clientId"
    )
    $versions = foreach ($key in $keys) {
        if (Test-Path -LiteralPath $key) {
            $value = (Get-ItemProperty -LiteralPath $key -Name pv -ErrorAction SilentlyContinue).pv
            if (-not [string]::IsNullOrWhiteSpace([string]$value)) {
                [string]$value
            }
        }
    }
    $selected = @($versions | Sort-Object -Unique | Select-Object -Last 1)
    if ($selected.Count -eq 0) {
        return 'not_detected'
    }
    return $selected[0]
}

function Get-RcEnvironmentEvidence {
    [CmdletBinding()]
    param()

    try {
        $operatingSystem = Get-CimInstance -ClassName Win32_OperatingSystem -ErrorAction Stop
        $computerSystem = Get-CimInstance -ClassName Win32_ComputerSystem -ErrorAction Stop
        $logicalProcessors = [int]$computerSystem.NumberOfLogicalProcessors
        $physicalMemory = [uint64]$computerSystem.TotalPhysicalMemory
        if ($logicalProcessors -lt 1 -or $physicalMemory -lt 1) {
            throw 'invalid_system_values'
        }
    } catch {
        throw 'environment_unavailable'
    }

    return [pscustomobject][ordered]@{
        windows_caption = [string]$operatingSystem.Caption
        windows_version = [string]$operatingSystem.Version
        windows_build = [string]$operatingSystem.BuildNumber
        logical_processors = $logicalProcessors
        physical_memory_bytes = $physicalMemory
        webview2_version = Get-RcWebView2Version
    }
}

function Select-RcProcessTree {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][object[]] $Rows,
        [Parameter(Mandatory)][ValidateRange(1, 2147483647)][int] $RootProcessId,
        [switch] $AllowMissingRoot
    )

    $rowById = @{}
    foreach ($row in $Rows) {
        $processId = [int]$row.ProcessId
        if ($processId -gt 0) {
            $rowById[$processId] = $row
        }
    }
    if (-not $rowById.ContainsKey($RootProcessId)) {
        if ($AllowMissingRoot) {
            return
        }
        throw 'desktop_process_missing'
    }

    # visited 同时限定递归边界和阻断损坏 CIM 数据中的父子环。
    $visited = [System.Collections.Generic.HashSet[int]]::new()
    $pending = [System.Collections.Generic.Queue[int]]::new()
    $pending.Enqueue($RootProcessId)
    while ($pending.Count -gt 0) {
        $current = $pending.Dequeue()
        if (-not $visited.Add($current)) {
            continue
        }
        foreach ($row in $Rows) {
            if ([int]$row.ParentProcessId -eq $current -and
                -not $visited.Contains([int]$row.ProcessId)) {
                $pending.Enqueue([int]$row.ProcessId)
            }
        }
    }
    foreach ($processId in @($visited | Sort-Object)) {
        $rowById[$processId]
    }
}

function Get-RcProcessSample {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][ValidateRange(1, 2147483647)][int] $DesktopProcessId,
        [switch] $AllowMissingRoot
    )

    try {
        # 显式属性列表避免意外读取命令行、可执行路径或其他用户敏感信息。
        $rows = @(Get-CimInstance -ClassName Win32_Process `
            -Property ProcessId, ParentProcessId, Name -ErrorAction Stop)
    } catch {
        throw 'process_observation_unavailable'
    }
    $tree = @(Select-RcProcessTree -Rows $rows -RootProcessId $DesktopProcessId `
        -AllowMissingRoot:$AllowMissingRoot)
    $samples = foreach ($row in $tree) {
        try {
            $process = Get-Process -Id ([int]$row.ProcessId) -ErrorAction Stop
            [pscustomobject][ordered]@{
                process_id = [int]$row.ProcessId
                parent_process_id = [int]$row.ParentProcessId
                process_name = ([System.IO.Path]::GetFileNameWithoutExtension([string]$row.Name)).ToLowerInvariant()
                total_processor_time_ms = [double]$process.TotalProcessorTime.TotalMilliseconds
                working_set_bytes = [uint64]$process.WorkingSet64
                private_bytes = [uint64]$process.PrivateMemorySize64
            }
        } catch {
            # 采样瞬间退出的进程由前后集合差表达，不能以零值伪装成存活进程。
        }
    }
    if (-not $AllowMissingRoot -and
        @($samples | Where-Object process_id -eq $DesktopProcessId).Count -ne 1) {
        throw 'desktop_process_missing'
    }
    return [pscustomobject][ordered]@{
        root_process_id = $DesktopProcessId
        processes = @($samples)
    }
}

function Get-RcProcessAggregate {
    param(
        [Parameter(Mandatory)][AllowEmptyCollection()][object[]] $AfterProcesses,
        [Parameter(Mandatory)][hashtable] $BeforeById,
        [Parameter(Mandatory)][double] $ElapsedSeconds,
        [Parameter(Mandatory)][int] $LogicalProcessors
    )

    $workingSet = [uint64]0
    $privateBytes = [uint64]0
    $processorDelta = [double]0
    $comparableCount = 0
    foreach ($process in $AfterProcesses) {
        $workingSet += [uint64]$process.working_set_bytes
        $privateBytes += [uint64]$process.private_bytes
        if ($BeforeById.ContainsKey([int]$process.process_id)) {
            $before = $BeforeById[[int]$process.process_id]
            $delta = [double]$process.total_processor_time_ms -
                [double]$before.total_processor_time_ms
            $processorDelta += [Math]::Max(0.0, $delta)
            $comparableCount++
        }
    }
    $cpuPercent = $null
    if ($comparableCount -gt 0) {
        $cpuPercent = [Math]::Round(
            100.0 * $processorDelta / ($ElapsedSeconds * 1000.0 * $LogicalProcessors),
            3
        )
    }
    return [pscustomobject][ordered]@{
        process_count = $AfterProcesses.Count
        cpu_percent = $cpuPercent
        working_set_bytes = $workingSet
        private_bytes = $privateBytes
    }
}

function Compare-RcProcessSamples {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] $Before,
        [Parameter(Mandatory)] $After,
        [Parameter(Mandatory)][ValidateRange(0.001, 86400.0)][double] $ElapsedSeconds,
        [Parameter(Mandatory)][ValidateRange(1, 65536)][int] $LogicalProcessors,
        [Parameter(Mandatory)][ValidateRange(1, 300)][int] $ObservationSeconds
    )

    $beforeById = @{}
    foreach ($process in @($Before.processes)) {
        $beforeById[[int]$process.process_id] = $process
    }
    $afterById = @{}
    foreach ($process in @($After.processes)) {
        $afterById[[int]$process.process_id] = $process
    }
    $rootId = [int]$Before.root_process_id
    $root = @($After.processes | Where-Object process_id -eq $rootId)
    $descendants = @($After.processes | Where-Object process_id -ne $rootId)
    $webview2 = @($descendants | Where-Object process_name -ceq 'msedgewebview2')
    $node = @($descendants | Where-Object process_name -ceq 'node')
    $newIds = @($afterById.Keys | Where-Object { -not $beforeById.ContainsKey($_) } | Sort-Object)
    $exitedIds = @($beforeById.Keys | Where-Object { -not $afterById.ContainsKey($_) } | Sort-Object)

    return [pscustomobject][ordered]@{
        status = 'passed'
        desktop_process_id = $rootId
        observation_seconds = $ObservationSeconds
        elapsed_seconds = [Math]::Round($ElapsedSeconds, 3)
        root_process = Get-RcProcessAggregate -AfterProcesses $root `
            -BeforeById $beforeById -ElapsedSeconds $ElapsedSeconds `
            -LogicalProcessors $LogicalProcessors
        descendants = Get-RcProcessAggregate -AfterProcesses $descendants `
            -BeforeById $beforeById -ElapsedSeconds $ElapsedSeconds `
            -LogicalProcessors $LogicalProcessors
        webview2 = Get-RcProcessAggregate -AfterProcesses $webview2 `
            -BeforeById $beforeById -ElapsedSeconds $ElapsedSeconds `
            -LogicalProcessors $LogicalProcessors
        node = Get-RcProcessAggregate -AfterProcesses $node `
            -BeforeById $beforeById -ElapsedSeconds $ElapsedSeconds `
            -LogicalProcessors $LogicalProcessors
        new_process_ids = $newIds
        exited_process_ids = $exitedIds
    }
}

function Measure-RcProcessTree {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][ValidateRange(1, 2147483647)][int] $DesktopProcessId,
        [ValidateRange(5, 300)][int] $ObservationSeconds = 60
    )

    $before = Get-RcProcessSample -DesktopProcessId $DesktopProcessId
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Seconds $ObservationSeconds
    $after = Get-RcProcessSample -DesktopProcessId $DesktopProcessId -AllowMissingRoot
    $stopwatch.Stop()
    return Compare-RcProcessSamples -Before $before -After $after `
        -ElapsedSeconds $stopwatch.Elapsed.TotalSeconds `
        -LogicalProcessors ([Environment]::ProcessorCount) `
        -ObservationSeconds $ObservationSeconds
}
