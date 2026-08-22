[CmdletBinding()]
param(
    [Parameter()]
    [ValidateNotNullOrEmpty()]
    [string] $Fixture,

    [Parameter()]
    [ValidateNotNullOrEmpty()]
    [string] $RuntimeDirectory,

    [Parameter()]
    [switch] $SecurityFixturesOnly
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw 'smoke-runtime.ps1 需要 PowerShell 7 或更高版本。'
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw '缺少开发命令: cargo'
}

$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$manifestPath = Join-Path $repositoryRoot 'src-tauri\Cargo.toml'
$temporaryRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
$smokeRoot = Join-Path $temporaryRoot "dsh-desktop-runtime-smoke\$([guid]::NewGuid().ToString('N'))"
$smokeFullPath = [System.IO.Path]::GetFullPath($smokeRoot)

# 每次验收只创建全新的临时审计目录。它既不是产品数据目录，也不会被脚本清理，避免
# 测试误触真实 DSH_HOME；测试遗留可由开发者在确认路径后另行处理。
if (-not $smokeFullPath.StartsWith($temporaryRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw '独立测试目录必须位于系统临时目录。'
}
foreach ($protectedBase in @($env:APPDATA, $env:LOCALAPPDATA)) {
    if (-not [string]::IsNullOrWhiteSpace($protectedBase)) {
        # 同时保护漫游数据、独立本地数据与 current-user NSIS 程序目录，避免未来
        # 调整临时目录策略时把烟雾夹具落入任何产品所有的根目录。
        foreach ($protectedName in @('DSH Desktop', 'DSH Desktop Data')) {
            $protectedRoot = [System.IO.Path]::GetFullPath(
                (Join-Path $protectedBase $protectedName)
            )
            if ($smokeFullPath.StartsWith(
                    $protectedRoot,
                    [System.StringComparison]::OrdinalIgnoreCase
                )) {
                throw '独立测试目录不能位于 DSH Desktop 用户数据或程序根。'
            }
        }
    }
}
New-Item -ItemType Directory -Path $smokeFullPath | Out-Null

function Invoke-CargoTest {
    param(
        [Parameter(Mandatory)]
        [string[]] $Arguments,

        [Parameter(Mandatory)]
        [string] $Description
    )

    Write-Host "[runtime smoke] $Description"
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "runtime smoke 失败: $Description (cargo exit $LASTEXITCODE)"
    }
}

$common = @('test', '--manifest-path', $manifestPath, '--locked')

# 每个用例均调用真实 Rust 边界：签名先于解析、流式摘要封存、网络中断重试、ZIP
# 路径规范化、probe 的 precommit 不变量，以及提交后启动失败的成对回滚。
Invoke-CargoTest -Description '签名错误拒绝' -Arguments ($common + @(
        '--test', 'manifest', 'signature_is_checked_against_exact_raw_bytes_before_parsing',
        '--', '--exact'
    ))
Invoke-CargoTest -Description '摘要错误拒绝' -Arguments ($common + @(
        '--lib', 'update::download::tests::rejects_digest_mismatch', '--', '--exact'
    ))
Invoke-CargoTest -Description '断网/连接中断失败关闭' -Arguments ($common + @(
        '--lib', 'update::download::tests::rejects_connection_closed_before_declared_body',
        '--', '--exact'
    ))
Invoke-CargoTest -Description '解压逃逸与 Windows 路径别名拒绝' -Arguments ($common + @(
        '--lib', 'update::archive::tests::rejects_paths_that_can_escape_or_alias_on_windows',
        '--', '--exact'
    ))
Invoke-CargoTest -Description 'probe 失败不改变 active deployment' -Arguments ($common + @(
        '--lib',
        'update::activation::tests::probe_rejection_is_explicit_precommit_and_preserves_prior_pointer',
        '--', '--exact'
    ))
Invoke-CargoTest -Description '候选首启失败回滚旧 runtime/data 对' -Arguments ($common + @(
        '--lib',
        'update::activation::tests::failed_first_start_restores_old_pair_and_starts_old_runtime_once',
        '--', '--exact'
    ))
Invoke-CargoTest -Description '全新安装首启失败保持 uninstalled' -Arguments ($common + @(
        '--lib',
        'update::activation::tests::fresh_install_failure_keeps_runtime_and_generation_but_persists_uninstalled',
        '--', '--exact'
    ))

# 皮肤安全门禁只复用仓库内的小型生成夹具，不读取用户选择的图片，也不依赖大 runtime
# ZIP。逐条精确调用可让 -SecurityFixturesOnly 在完整 Rust 套件之外快速暴露关键边界回归。
Invoke-CargoTest -Description '皮肤图片格式错误拒绝' -Arguments ($common + @(
        '--test', 'skin_import',
        'rejects_corruption_and_unsupported_gif_with_distinct_kinds', '--', '--exact'
    ))
Invoke-CargoTest -Description '皮肤图片尺寸上限拒绝' -Arguments ($common + @(
        '--test', 'skin_import',
        'rejects_over_edge_and_total_pixel_limits_before_decode', '--', '--exact'
    ))
Invoke-CargoTest -Description '皮肤协议遍历与非规范 URI 拒绝' -Arguments ($common + @(
        '--test', 'skin_protocol',
        'rejects_every_noncanonical_uri_without_reflecting_request_text', '--', '--exact'
    ))
Invoke-CargoTest -Description '不支持的 DSH 适配器仅执行清理回退' -Arguments ($common + @(
        '--test', 'skin_adapter',
        'page_plan_requires_exact_version_and_numeric_loopback_origin', '--', '--exact'
    ))
Invoke-CargoTest -Description '官方页面仅获得失败关闭的皮肤报告命令' -Arguments ($common + @(
        '--test', 'command_permissions',
        'official_main_receives_only_the_fail_closed_adapter_report_command', '--', '--exact'
    ))

if ($SecurityFixturesOnly) {
    Write-Host "小型 runtime/皮肤安全夹具通过；独立审计目录: $smokeFullPath"
    exit 0
}

if ([string]::IsNullOrWhiteSpace($Fixture)) {
    throw '大 runtime smoke 必须显式传入 -Fixture <runtime.zip>。'
}
if (-not (Test-Path -LiteralPath $Fixture -PathType Leaf)) {
    throw 'Fixture 不存在或不是文件。'
}
$fixturePath = (Resolve-Path -LiteralPath $Fixture).Path
if ([System.IO.Path]::GetExtension($fixturePath) -ine '.zip') {
    throw 'Fixture 必须是 runtime ZIP。'
}

Write-Host '[runtime smoke] 只读核对大 runtime ZIP inventory closure'
$fixtureDigest = (Get-FileHash -LiteralPath $fixturePath -Algorithm SHA256).Hash.ToLowerInvariant()
$fixtureStream = [System.IO.File]::OpenRead($fixturePath)
try {
    $archive = [System.IO.Compression.ZipArchive]::new(
        $fixtureStream,
        [System.IO.Compression.ZipArchiveMode]::Read,
        $false
    )
    try {
        $inventoryEntries = @($archive.Entries | Where-Object { $_.FullName -ceq 'inventory.json' })
        if ($inventoryEntries.Count -ne 1) {
            throw 'runtime ZIP 必须且只能包含一份 inventory.json。'
        }
        $reader = [System.IO.StreamReader]::new($inventoryEntries[0].Open())
        try {
            $inventory = @($reader.ReadToEnd() | ConvertFrom-Json -Depth 20)
        }
        finally {
            $reader.Dispose()
        }
        $inventoryStream = $inventoryEntries[0].Open()
        try {
            $inventoryDigest = [Convert]::ToHexString(
                [System.Security.Cryptography.SHA256]::HashData($inventoryStream)
            ).ToLowerInvariant()
        }
        finally {
            $inventoryStream.Dispose()
        }
        if ($inventory.Count -eq 0) {
            throw 'runtime inventory 不能为空。'
        }

        $expected = [System.Collections.Generic.Dictionary[string, object]]::new(
            [System.StringComparer]::OrdinalIgnoreCase
        )
        foreach ($item in $inventory) {
            if ($item.path -isnot [string] -or $item.path -notmatch '^[^\\/:]+(?:/[^\\/:]+)*$' -or
                $item.path.Split('/') -contains '..' -or $item.sha256 -notmatch '^[0-9a-f]{64}$' -or
                $item.size -isnot [long] -or $item.size -lt 0) {
                throw 'runtime inventory 包含无效路径、大小或摘要。'
            }
            if (-not $expected.TryAdd($item.path, $item)) {
                throw 'runtime inventory 包含大小写折叠后的重复路径。'
            }
        }

        $seen = [System.Collections.Generic.HashSet[string]]::new(
            [System.StringComparer]::OrdinalIgnoreCase
        )
        foreach ($entry in $archive.Entries) {
            $normalized = $entry.FullName.TrimEnd('/')
            if ([string]::IsNullOrWhiteSpace($normalized) -or $normalized -match '\\' -or
                $normalized -match '^(?:/|[A-Za-z]:)' -or
                $normalized.Split('/') -contains '..') {
                throw 'runtime ZIP 包含可能逃逸或产生别名的路径。'
            }
            if ([string]::IsNullOrEmpty($entry.Name)) {
                continue
            }
            if (-not $seen.Add($entry.FullName)) {
                throw 'runtime ZIP 包含大小写折叠后的重复文件路径。'
            }
            if ($entry.FullName -ceq 'inventory.json') {
                continue
            }
            $item = $null
            if (-not $expected.TryGetValue($entry.FullName, [ref] $item)) {
                throw 'runtime ZIP 包含 inventory 未声明的文件。'
            }
            if ($entry.Length -ne $item.size) {
                throw 'runtime ZIP 文件大小与 inventory 不一致。'
            }
            $entryStream = $entry.Open()
            try {
                $actualDigest = [Convert]::ToHexString(
                    [System.Security.Cryptography.SHA256]::HashData($entryStream)
                ).ToLowerInvariant()
            }
            finally {
                $entryStream.Dispose()
            }
            if ($actualDigest -cne $item.sha256) {
                throw 'runtime ZIP 文件摘要与 inventory 不一致。'
            }
        }
        if ($seen.Count -ne ($expected.Count + 1)) {
            throw 'runtime ZIP 缺少 inventory 声明的文件。'
        }
        foreach ($required in @(
                'app/node_modules/@deepseek-ai/dsh/package.json',
                'app/node_modules/@deepseek-ai/dsh/lib/bin.js'
            )) {
            if (-not $seen.Contains($required)) {
                throw "runtime ZIP 缺少必需文件: $required"
            }
        }
        if (-not ($seen | Where-Object { $_ -match '^node-v\d+\.\d+\.\d+-win-x64/node\.exe$' })) {
            throw 'runtime ZIP 缺少固定 Windows x64 Node 可执行文件。'
        }
    }
    finally {
        $archive.Dispose()
    }
}
finally {
    $fixtureStream.Dispose()
}

$runtimeReadyMs = $null
if (-not [string]::IsNullOrWhiteSpace($RuntimeDirectory)) {
    if (-not (Test-Path -LiteralPath $RuntimeDirectory -PathType Container)) {
        throw 'RuntimeDirectory 不存在或不是目录。'
    }
    $runtimeRootItem = Get-Item -LiteralPath $RuntimeDirectory -Force
    if (($runtimeRootItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw 'RuntimeDirectory 根目录不能是 junction 或 symlink。'
    }
    $runtimePath = (Resolve-Path -LiteralPath $RuntimeDirectory).Path
    $runtimeInventoryPath = Join-Path $runtimePath 'inventory.json'
    if (-not (Test-Path -LiteralPath $runtimeInventoryPath -PathType Leaf)) {
        throw 'RuntimeDirectory 缺少与 Fixture 绑定的 inventory.json。'
    }

    # RuntimeDirectory 不是独立的“可信输入”。逐文件复核相同 inventory 的闭包、大小与
    # SHA-256，避免坏 ZIP 与旧的正常解压目录组合后形成假阳性的 Web 探活证据。
    Write-Host '[runtime smoke] 绑定 RuntimeDirectory 与 Fixture inventory'
    $runtimeInventoryDigest = (
        Get-FileHash -LiteralPath $runtimeInventoryPath -Algorithm SHA256
    ).Hash.ToLowerInvariant()
    if ($runtimeInventoryDigest -cne $inventoryDigest) {
        throw 'RuntimeDirectory inventory.json 与 Fixture 不一致。'
    }
    $runtimeSeen = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )
    foreach ($directory in Get-ChildItem -LiteralPath $runtimePath -Recurse -Directory) {
        if (($directory.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw 'RuntimeDirectory 包含不允许的 reparse directory。'
        }
    }
    foreach ($runtimeFile in Get-ChildItem -LiteralPath $runtimePath -Recurse -File) {
        if (($runtimeFile.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw 'RuntimeDirectory 包含不允许的 reparse file。'
        }
        $relative = [System.IO.Path]::GetRelativePath(
            $runtimePath,
            $runtimeFile.FullName
        ).Replace('\', '/')
        if (-not $runtimeSeen.Add($relative)) {
            throw 'RuntimeDirectory 包含大小写折叠后的重复文件路径。'
        }
        if ($relative -ceq 'inventory.json') {
            continue
        }
        $runtimeItem = $null
        if (-not $expected.TryGetValue($relative, [ref] $runtimeItem)) {
            throw 'RuntimeDirectory 包含 Fixture inventory 未声明的文件。'
        }
        if ($runtimeFile.Length -ne $runtimeItem.size) {
            throw 'RuntimeDirectory 文件大小与 Fixture inventory 不一致。'
        }
        $runtimeDigest = (
            Get-FileHash -LiteralPath $runtimeFile.FullName -Algorithm SHA256
        ).Hash.ToLowerInvariant()
        if ($runtimeDigest -cne $runtimeItem.sha256) {
            throw 'RuntimeDirectory 文件摘要与 Fixture inventory 不一致。'
        }
    }
    if ($runtimeSeen.Count -ne ($expected.Count + 1)) {
        throw 'RuntimeDirectory 缺少 Fixture inventory 声明的文件。'
    }

    $nodeFiles = @(Get-ChildItem -LiteralPath $runtimePath -Directory | Where-Object {
            $_.Name -match '^node-v\d+\.\d+\.\d+-win-x64$'
        } | ForEach-Object {
            Get-Item -LiteralPath (Join-Path $_.FullName 'node.exe') -ErrorAction SilentlyContinue
        })
    if ($nodeFiles.Count -ne 1) {
        throw 'RuntimeDirectory 必须且只能包含一个固定版本的 Windows x64 Node。'
    }
    $cliPath = Join-Path $runtimePath 'app\node_modules\@deepseek-ai\dsh\lib\bin.js'
    if (-not (Test-Path -LiteralPath $cliPath -PathType Leaf)) {
        throw 'RuntimeDirectory 缺少 DSH CLI。'
    }

    # 真实 Web smoke 仅把 DSH_HOME 指向本轮独立目录；runtime 闭包保持只读。输出仅用于
    # 精确就绪信号判断，不写入报告或终端，避免泄露配置与用户正文。
    $probeListener = [System.Net.Sockets.TcpListener]::new(
        [System.Net.IPAddress]::Loopback,
        0
    )
    $probeListener.Start()
    $smokePort = ([System.Net.IPEndPoint] $probeListener.LocalEndpoint).Port
    $probeListener.Stop()
    $runtimeHome = Join-Path $smokeFullPath 'dsh-home'
    New-Item -ItemType Directory -Path $runtimeHome | Out-Null

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $nodeFiles[0].FullName
    $startInfo.WorkingDirectory = $runtimePath
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.Environment['DSH_HOME'] = $runtimeHome
    $startInfo.Environment['NO_COLOR'] = '1'
    foreach ($argument in @(
            $cliPath,
            'web',
            '--host',
            '127.0.0.1',
            '--port',
            $smokePort.ToString(),
            '--no-open'
        )) {
        $startInfo.ArgumentList.Add($argument)
    }

    Write-Host '[runtime smoke] 启动真实 DSH WebUI 双门探活'
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    $runtimeProcess = [System.Diagnostics.Process]::Start($startInfo)
    $stdoutDrain = $runtimeProcess.StandardOutput.ReadToEndAsync()
    $stderrDrain = $runtimeProcess.StandardError.ReadToEndAsync()
    $ready = $false
    $runtimeStdout = ''
    try {
        $deadline = [DateTimeOffset]::UtcNow.AddSeconds(30)
        while ([DateTimeOffset]::UtcNow -lt $deadline -and -not $runtimeProcess.HasExited) {
            try {
                $response = Invoke-WebRequest `
                    -Uri "http://127.0.0.1:$smokePort/" `
                    -TimeoutSec 1 `
                    -UseBasicParsing
                if ($response.StatusCode -eq 200) {
                    $ready = $true
                    $runtimeReadyMs = $stopwatch.ElapsedMilliseconds
                    break
                }
            }
            catch [System.Net.Http.HttpRequestException] {
                Start-Sleep -Milliseconds 100
            }
            catch [System.Net.WebException] {
                Start-Sleep -Milliseconds 100
            }
            catch [System.TimeoutException] {
                Start-Sleep -Milliseconds 100
            }
        }
        if (-not $ready) {
            throw '真实 DSH WebUI 未在 30 秒内就绪。'
        }
    }
    finally {
        $stopwatch.Stop()
        if (-not $runtimeProcess.HasExited) {
            $runtimeProcess.Kill($true)
        }
        $runtimeProcess.WaitForExit()
        $runtimeStdout = $stdoutDrain.GetAwaiter().GetResult()
        $stderrDrain.GetAwaiter().GetResult() | Out-Null
    }
    $expectedReadiness = "dsh web: http://127.0.0.1:$smokePort"
    if (-not (($runtimeStdout -split '\r?\n') -ccontains $expectedReadiness)) {
        throw '真实 DSH WebUI stdout 就绪信号缺失。'
    }
}

$report = [ordered]@{
    schema = 1
    fixture = $fixturePath
    fixture_sha256 = $fixtureDigest
    runtime_inventory_sha256 = if ($null -eq $runtimeReadyMs) { $null } else { $inventoryDigest }
    runtime_ready_ms = $runtimeReadyMs
    verified_at = [DateTimeOffset]::UtcNow.ToString('O')
    result = 'passed'
}
$report | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $smokeFullPath 'report.json') -Encoding utf8
Write-Host "大 runtime 只读验收通过；SHA-256: $fixtureDigest"
Write-Host "独立审计目录（脚本未删除）: $smokeFullPath"
