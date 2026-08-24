Set-StrictMode -Version Latest

$script:RcStatuses = @('passed', 'failed', 'not_run')

function New-RcCheckList {
    [CmdletBinding()]
    param()

    # 固定 ID 是跨 RC 对比的稳定主键；中文说明可以改进，但不能用显示文本充当状态键。
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
    if (@($Checks | Where-Object status -ceq 'failed').Count -gt 0) {
        return 'failed'
    }
    if ($Checks.Count -eq 0 -or
        @($Checks | Where-Object status -ceq 'not_run').Count -gt 0) {
        return 'not_run'
    }
    return 'passed'
}

function ConvertTo-RcSafeProcessAggregate {
    param([Parameter(Mandatory)] $Aggregate)

    return [ordered]@{
        process_count = [int]$Aggregate.process_count
        cpu_percent = if ($null -eq $Aggregate.cpu_percent) {
            $null
        } else {
            [double]$Aggregate.cpu_percent
        }
        working_set_bytes = [uint64]$Aggregate.working_set_bytes
        private_bytes = [uint64]$Aggregate.private_bytes
    }
}

function ConvertTo-RcSafeEvidence {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Evidence)

    # 先重建固定 schema，而不是清理任意输入；这样新增动态字段默认不会进入审计文件。
    $safeChecks = @($Evidence.checks | ForEach-Object {
        if ($_.status -cnotin $script:RcStatuses) {
            throw 'rc_status_invalid'
        }
        [ordered]@{
            id = [string]$_.id
            description = [string]$_.description
            status = [string]$_.status
            evidence = [string]$_.evidence
        }
    })

    $process = $Evidence.process_observation
    $safeProcess = [ordered]@{ status = [string]$process.status }
    if ([string]$process.status -ceq 'passed') {
        $safeProcess.desktop_process_id = [int]$process.desktop_process_id
        $safeProcess.observation_seconds = [int]$process.observation_seconds
        $safeProcess.elapsed_seconds = [double]$process.elapsed_seconds
        foreach ($name in @('root_process', 'descendants', 'webview2', 'node')) {
            $safeProcess[$name] = ConvertTo-RcSafeProcessAggregate -Aggregate $process.$name
        }
        $safeProcess.new_process_ids = @($process.new_process_ids | ForEach-Object { [int]$_ })
        $safeProcess.exited_process_ids = @($process.exited_process_ids | ForEach-Object { [int]$_ })
    }

    return [ordered]@{
        schema_version = [int]$Evidence.schema_version
        generated_at_utc = [string]$Evidence.generated_at_utc
        result = [string]$Evidence.result
        build = [ordered]@{
            desktop_version = [string]$Evidence.build.desktop_version
            git_commit = [string]$Evidence.build.git_commit
        }
        environment = [ordered]@{
            windows_caption = [string]$Evidence.environment.windows_caption
            windows_version = [string]$Evidence.environment.windows_version
            windows_build = [string]$Evidence.environment.windows_build
            logical_processors = [int]$Evidence.environment.logical_processors
            physical_memory_bytes = [uint64]$Evidence.environment.physical_memory_bytes
            webview2_version = [string]$Evidence.environment.webview2_version
        }
        installer = [ordered]@{
            file_name = [string]$Evidence.installer.file_name
            size_bytes = [uint64]$Evidence.installer.size_bytes
            sha256 = [string]$Evidence.installer.sha256
            authenticode = [string]$Evidence.installer.authenticode
        }
        process_observation = $safeProcess
        checks = $safeChecks
    }
}

function ConvertTo-RcMarkdown {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Evidence)

    $safe = ConvertTo-RcSafeEvidence -Evidence $Evidence
    $lines = [System.Collections.Generic.List[string]]::new()
    $lines.Add('# DSH Desktop Windows RC 验收报告')
    $lines.Add('')
    $lines.Add("- 生成时间（UTC）：$($safe.generated_at_utc)")
    $lines.Add("- 整体状态：$($safe.result)")
    $lines.Add("- Desktop 版本：$($safe.build.desktop_version)")
    $lines.Add("- Git commit：$($safe.build.git_commit)")
    $lines.Add("- Windows：$($safe.environment.windows_caption) $($safe.environment.windows_version) build $($safe.environment.windows_build)")
    $lines.Add("- WebView2：$($safe.environment.webview2_version)")
    $lines.Add("- 安装包：$($safe.installer.file_name)")
    $lines.Add("- 安装包字节数：$($safe.installer.size_bytes)")
    $lines.Add("- 安装包 SHA-256：$($safe.installer.sha256)")
    $lines.Add("- Authenticode：$($safe.installer.authenticode)")
    $lines.Add('')
    $lines.Add('## 进程观测')
    $lines.Add('')
    $lines.Add("- 状态：$($safe.process_observation.status)")
    if ($safe.process_observation.Contains('observation_seconds')) {
        $lines.Add("- 请求采样秒数：$($safe.process_observation.observation_seconds)")
    }
    $lines.Add('')
    $lines.Add('## 验收检查')
    $lines.Add('')
    $lines.Add('| ID | 检查项 | 状态 | 证据 |')
    $lines.Add('| --- | --- | --- | --- |')
    foreach ($check in $safe.checks) {
        # 人工证据只允许单行显示，避免破坏 Markdown 表格结构。
        $description = $check.description.Replace('|', '\|').Replace("`r", ' ').Replace("`n", ' ')
        $evidenceText = $check.evidence.Replace('|', '\|').Replace("`r", ' ').Replace("`n", ' ')
        $lines.Add("| $($check.id) | $description | $($check.status) | $evidenceText |")
    }
    return ($lines -join "`n") + "`n"
}

function Write-RcCreateNewUtf8 {
    param(
        [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][string] $Content
    )

    try {
        $stream = [System.IO.FileStream]::new(
            $Path,
            [System.IO.FileMode]::CreateNew,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::None
        )
        try {
            $bytes = [System.Text.UTF8Encoding]::new($false).GetBytes($Content)
            $stream.Write($bytes, 0, $bytes.Length)
            $stream.Flush($true)
        } finally {
            $stream.Dispose()
        }
    } catch [System.IO.IOException] {
        throw 'rc_evidence_exists'
    }
}

function Write-RcEvidenceFiles {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] $Evidence,
        [Parameter(Mandatory)][string] $AuditDirectory
    )

    $jsonPath = Join-Path $AuditDirectory 'evidence.json'
    $markdownPath = Join-Path $AuditDirectory 'report.md'
    if ((Test-Path -LiteralPath $jsonPath) -or (Test-Path -LiteralPath $markdownPath)) {
        throw 'rc_evidence_exists'
    }

    $safe = ConvertTo-RcSafeEvidence -Evidence $Evidence
    $json = ($safe | ConvertTo-Json -Depth 12) + "`n"
    $markdown = ConvertTo-RcMarkdown -Evidence $safe
    Write-RcCreateNewUtf8 -Path $jsonPath -Content $json
    Write-RcCreateNewUtf8 -Path $markdownPath -Content $markdown
}
