[CmdletBinding(SupportsShouldProcess)]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $DshVersion,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $NodeArchive,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $NodeSha256,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $OutputDirectory
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-BuildInputs {
    if ($DshVersion -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z]+(?:\.[0-9A-Za-z]+)*)?$') {
        throw 'DshVersion 必须是 exact semver，禁止 latest、npx 或全局安装命令。'
    }
    if ([System.IO.Path]::GetFileName($NodeArchive) -notmatch '^node-v(?<version>\d+\.\d+\.\d+)-win-x64\.zip$') {
        throw 'NodeArchive 必须是官方 node-v<version>-win-x64.zip。'
    }
    if ($NodeSha256 -notmatch '^[0-9a-fA-F]{64}$') {
        throw 'NodeSha256 必须是 64 位 SHA-256 hex。'
    }
}

Assert-BuildInputs

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$lockDirectory = Join-Path $repositoryRoot "runtime\locks\dsh-$DshVersion"
$lockPath = Join-Path $lockDirectory 'package-lock.json'
$packagePath = Join-Path $lockDirectory 'package.json'

if (-not (Test-Path -LiteralPath $lockPath) -or -not (Test-Path -LiteralPath $packagePath)) {
    throw "缺少已审查的 runtime lock：dsh-$DshVersion"
}

$lock = Get-Content -Raw -LiteralPath $lockPath | ConvertFrom-Json -AsHashtable -Depth 100
$rootVersion = $lock['packages']['']['dependencies']['@deepseek-ai/dsh']
$resolvedVersion = $lock['packages']['node_modules/@deepseek-ai/dsh']['version']
if ($rootVersion -ne $DshVersion -or $resolvedVersion -ne $DshVersion) {
    throw 'package-lock root 或已解析 DSH 版本与 DshVersion 不符。'
}

$archivePath = [System.IO.Path]::GetFullPath($NodeArchive)
if (-not (Test-Path -LiteralPath $archivePath -PathType Leaf)) {
    throw 'NodeArchive 本地文件不存在。'
}
$actualDigest = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
if ($actualDigest -ne $NodeSha256) {
    throw 'NodeArchive SHA-256 摘要不符。'
}

$nodeVersion = [regex]::Match([System.IO.Path]::GetFileName($archivePath), '^node-v(?<version>\d+\.\d+\.\d+)-win-x64\.zip$').Groups['version'].Value
$outputRoot = [System.IO.Path]::GetFullPath($OutputDirectory)
$stageRoot = Join-Path $outputRoot "stage\dsh-$DshVersion-node-$nodeVersion-win-x64"
$archiveOutput = Join-Path $outputRoot "dsh-runtime-$DshVersion-node-$nodeVersion-win-x64.zip"

if ($WhatIfPreference) {
    Write-Output "WhatIf: validate local Node archive and committed lock; stage=$stageRoot; output=$archiveOutput"
    return
}

if ((Test-Path -LiteralPath $stageRoot) -or (Test-Path -LiteralPath $archiveOutput)) {
    throw '固定 staging 或输出已存在；请使用新的空 runtime-out 目录。'
}
if (-not $PSCmdlet.ShouldProcess($archiveOutput, '制作兼容 DSH runtime')) {
    return
}

New-Item -ItemType Directory -Path $stageRoot | Out-Null
Expand-Archive -LiteralPath $archivePath -DestinationPath $stageRoot
$nodeRoot = Join-Path $stageRoot "node-v$nodeVersion-win-x64"
if (-not (Test-Path -LiteralPath (Join-Path $nodeRoot 'node.exe') -PathType Leaf)) {
    throw 'Node archive 目录结构无效。'
}

$appRoot = Join-Path $stageRoot 'app'
New-Item -ItemType Directory -Path $appRoot | Out-Null
Copy-Item -LiteralPath $packagePath, $lockPath -Destination $appRoot
$npmCli = Join-Path $nodeRoot 'node_modules\npm\bin\npm-cli.js'
& (Join-Path $nodeRoot 'node.exe') $npmCli ci --prefix $appRoot --omit=dev --ignore-scripts --no-audit --no-fund --legacy-peer-deps
if ($LASTEXITCODE -ne 0) { throw 'npm ci 安装 runtime 失败。' }

$installedPackagePath = Join-Path $appRoot 'node_modules\@deepseek-ai\dsh\package.json'
$installed = Get-Content -Raw -LiteralPath $installedPackagePath | ConvertFrom-Json
if ($installed.name -ne '@deepseek-ai/dsh' -or $installed.version -ne $DshVersion) {
    throw '安装后的 DSH package name/version 与 lock 不符。'
}

$dshBin = Join-Path $appRoot 'node_modules\@deepseek-ai\dsh\lib\bin.js'
& (Join-Path $nodeRoot 'node.exe') $dshBin --help | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'DSH --help smoke 失败。' }

# 使用动态回环端口做真实 Web 启停探测；独立 smoke home 不会进入最终压缩包。
$probeListener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$probeListener.Start()
$smokePort = ([System.Net.IPEndPoint] $probeListener.LocalEndpoint).Port
$probeListener.Stop()
$smokeHome = Join-Path $outputRoot "smoke-home-$DshVersion"
New-Item -ItemType Directory -Path $smokeHome | Out-Null

$startInfo = [System.Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = Join-Path $nodeRoot 'node.exe'
$startInfo.UseShellExecute = $false
$startInfo.CreateNoWindow = $true
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
$startInfo.Environment['DSH_HOME'] = $smokeHome
$startInfo.Environment['NO_COLOR'] = '1'
foreach ($argument in @($dshBin, 'web', '--host', '127.0.0.1', '--port', $smokePort.ToString())) {
    $startInfo.ArgumentList.Add($argument)
}
$smokeProcess = [System.Diagnostics.Process]::Start($startInfo)
$stdoutDrain = $smokeProcess.StandardOutput.ReadToEndAsync()
$stderrDrain = $smokeProcess.StandardError.ReadToEndAsync()
try {
    $deadline = [DateTimeOffset]::UtcNow.AddSeconds(20)
    $ready = $false
    while ([DateTimeOffset]::UtcNow -lt $deadline -and -not $smokeProcess.HasExited) {
        try {
            $response = Invoke-WebRequest -Uri "http://127.0.0.1:$smokePort/" -TimeoutSec 1 -UseBasicParsing
            if ($response.StatusCode -eq 200) {
                $ready = $true
                break
            }
        }
        catch [System.Net.Http.HttpRequestException] {
            Start-Sleep -Milliseconds 100
        }
        catch [System.Net.WebException] {
            Start-Sleep -Milliseconds 100
        }
    }
    if (-not $ready) { throw 'DSH Web 回环 smoke 未在 20 秒内就绪。' }
}
finally {
    if (-not $smokeProcess.HasExited) {
        $smokeProcess.Kill($true)
    }
    $smokeProcess.WaitForExit()
    $stdoutDrain.GetAwaiter().GetResult() | Out-Null
    $stderrDrain.GetAwaiter().GetResult() | Out-Null
}

# notices 先落盘，使它本身也进入随后生成的 payload 文件摘要清单。
$notices = Get-ChildItem -LiteralPath (Join-Path $appRoot 'node_modules') -Recurse -Filter package.json | ForEach-Object {
    $package = Get-Content -Raw -LiteralPath $_.FullName | ConvertFrom-Json
    [ordered]@{ name = $package.name; version = $package.version; license = $package.license }
} | Sort-Object name, version
$notices | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $stageRoot 'THIRD_PARTY_NOTICES.json') -Encoding utf8NoBOM

# inventory 覆盖全部 runtime payload；它不能包含自身摘要，发布方另行摘要最终 ZIP。
$inventory = Get-ChildItem -LiteralPath $stageRoot -Recurse -File | ForEach-Object {
    [ordered]@{
        path = [System.IO.Path]::GetRelativePath($stageRoot, $_.FullName).Replace('\', '/')
        size = $_.Length
        sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    }
}
$inventory | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $stageRoot 'inventory.json') -Encoding utf8NoBOM

Compress-Archive -Path (Join-Path $stageRoot '*') -DestinationPath $archiveOutput -CompressionLevel Optimal
Write-Output $archiveOutput
