$ErrorActionPreference = 'Stop'

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$scriptPath = Join-Path $repositoryRoot 'scripts\build-runtime.ps1'

function Invoke-BuildRuntimeTest {
    param(
        [Parameter(Mandatory)]
        [string[]] $Arguments,
        [string] $TargetScript = $scriptPath
    )

    $output = & pwsh -NoProfile -File $TargetScript @Arguments 2>&1 | Out-String
    return [pscustomobject]@{
        ExitCode = $LASTEXITCODE
        Output = $output
    }
}

function Assert-FailsWith {
    param(
        [Parameter(Mandatory)]
        [string[]] $Arguments,
        [Parameter(Mandatory)]
        [string] $Pattern,
        [string] $TargetScript = $scriptPath
    )

    $result = Invoke-BuildRuntimeTest -Arguments $Arguments -TargetScript $TargetScript
    if ($result.ExitCode -eq 0 -or $result.Output -notmatch $Pattern) {
        throw "预期失败并匹配 '$Pattern'，实际 exit=$($result.ExitCode)，output=$($result.Output)"
    }
}

# 参数校验必须发生在任何网络或大文件操作之前，因此这些用例只使用不存在的本地路径。
Assert-FailsWith -Arguments @('-WhatIf') -Pattern 'DshVersion'
Assert-FailsWith -Arguments @('-DshVersion', 'latest', '-NodeArchive', 'missing.zip', '-NodeSha256', ('0' * 64), '-OutputDirectory', 'out', '-WhatIf') -Pattern 'exact|精确'
Assert-FailsWith -Arguments @('-DshVersion', 'npx @deepseek-ai/dsh', '-NodeArchive', 'missing.zip', '-NodeSha256', ('0' * 64), '-OutputDirectory', 'out', '-WhatIf') -Pattern 'exact|精确'
Assert-FailsWith -Arguments @('-DshVersion', 'npm install -g @deepseek-ai/dsh@0.1.1-rc.1', '-NodeArchive', 'missing.zip', '-NodeSha256', ('0' * 64), '-OutputDirectory', 'out', '-WhatIf') -Pattern 'exact|精确'
Assert-FailsWith -Arguments @('-DshVersion', '0.1.1-rc.1', '-NodeArchive', 'node-v24.15.0-win-arm64.zip', '-NodeSha256', ('0' * 64), '-OutputDirectory', 'out', '-WhatIf') -Pattern 'x64'
Assert-FailsWith -Arguments @('-DshVersion', '0.1.1-rc.1', '-NodeArchive', 'node-v24.15.0-win-x64.zip', '-NodeSha256', ('g' * 64), '-OutputDirectory', 'out', '-WhatIf') -Pattern 'SHA-256'

# 隔离仓库夹具证明摘要比较与 lock root 校验是真实行为，不修改已审查 lock。
$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) "dsh-runtime-build-test-$([guid]::NewGuid().ToString('N'))"
$testScripts = Join-Path $testRoot 'scripts'
$testLock = Join-Path $testRoot 'runtime\locks\dsh-0.1.1-rc.1'
New-Item -ItemType Directory -Path $testScripts, $testLock | Out-Null
$isolatedScript = Join-Path $testScripts 'build-runtime.ps1'
Copy-Item -LiteralPath $scriptPath -Destination $isolatedScript
$nodeArchive = Join-Path $testRoot 'node-v24.15.0-win-x64.zip'
Set-Content -LiteralPath $nodeArchive -Value 'not-a-real-node-archive' -NoNewline
Set-Content -LiteralPath (Join-Path $testLock 'package.json') -Value '{"private":true,"dependencies":{"@deepseek-ai/dsh":"0.1.1-rc.1"}}' -NoNewline
$validLock = '{"lockfileVersion":3,"packages":{"":{"dependencies":{"@deepseek-ai/dsh":"0.1.1-rc.1"}},"node_modules/@deepseek-ai/dsh":{"name":"@deepseek-ai/dsh","version":"0.1.1-rc.1"}}}'
Set-Content -LiteralPath (Join-Path $testLock 'package-lock.json') -Value $validLock -NoNewline
Set-Content -LiteralPath (Join-Path $testLock 'install-scripts.json') -Value '{"schema":1,"packages":[]}' -NoNewline
Assert-FailsWith -TargetScript $isolatedScript -Arguments @('-DshVersion', '0.1.1-rc.1', '-NodeArchive', $nodeArchive, '-NodeSha256', ('0' * 64), '-OutputDirectory', (Join-Path $testRoot 'out'), '-WhatIf') -Pattern '摘要不符'

$wrongLock = $validLock.Replace('"0.1.1-rc.1"}}}', '"0.1.2"}}}')
Set-Content -LiteralPath (Join-Path $testLock 'package-lock.json') -Value $wrongLock -NoNewline
Assert-FailsWith -TargetScript $isolatedScript -Arguments @('-DshVersion', '0.1.1-rc.1', '-NodeArchive', $nodeArchive, '-NodeSha256', ('0' * 64), '-OutputDirectory', (Join-Path $testRoot 'out'), '-WhatIf') -Pattern 'lock|版本'

$scriptedLock = '{"lockfileVersion":3,"packages":{"":{"dependencies":{"@deepseek-ai/dsh":"0.1.1-rc.1"}},"node_modules/@deepseek-ai/dsh":{"name":"@deepseek-ai/dsh","version":"0.1.1-rc.1"},"node_modules/native-addon":{"name":"native-addon","version":"1.0.0","integrity":"sha512-test","hasInstallScript":true}}}'
Set-Content -LiteralPath (Join-Path $testLock 'package-lock.json') -Value $scriptedLock -NoNewline
$approvedNative = '{"path":"node_modules/native-addon","name":"native-addon","version":"1.0.0","integrity":"sha512-test"}'
$validNativeAllowlist = "{`"schema`":1,`"packages`":[${approvedNative}]}"
foreach ($mismatchedAllowlist in @(
    '{"schema":1,"packages":[]}',
    "{`"schema`":1,`"packages`":[${approvedNative},{`"path`":`"node_modules/extra`",`"name`":`"extra`",`"version`":`"1.0.0`",`"integrity`":`"sha512-extra`"}]}",
    $validNativeAllowlist.Replace('"version":"1.0.0"', '"version":"2.0.0"'),
    $validNativeAllowlist.Replace('sha512-test', 'sha512-tampered')
)) {
    Set-Content -LiteralPath (Join-Path $testLock 'install-scripts.json') -Value $mismatchedAllowlist -NoNewline
    Assert-FailsWith -TargetScript $isolatedScript -Arguments @('-DshVersion', '0.1.1-rc.1', '-NodeArchive', $nodeArchive, '-NodeSha256', ('0' * 64), '-OutputDirectory', (Join-Path $testRoot 'out'), '-WhatIf') -Pattern 'install|白名单|allowlist'
}

# 极小假 Node 只模拟发布脚本依赖的三个外部边界：npm ci、CLI help、回环 Web 200。
$fakeRoot = Join-Path $testRoot 'fake-node-archive\node-v24.15.0-win-x64'
New-Item -ItemType Directory -Path (Join-Path $fakeRoot 'node_modules\npm\bin') | Out-Null
Set-Content -LiteralPath (Join-Path $fakeRoot 'node_modules\npm\bin\npm-cli.js') -Value '' -NoNewline
$fakeNodeSource = @'
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.get(0).is_some_and(|value| value.ends_with("npm-cli.js"))
        && args.get(1).is_some_and(|value| value == "ci")
    {
        if args.iter().any(|value| value == "--ignore-scripts") { std::process::exit(10); }
        if args.iter().any(|value| value == "--legacy-peer-deps") { std::process::exit(12); }
        let prefix = PathBuf::from(&args[args.iter().position(|value| value == "--prefix").unwrap() + 1]);
        let package = prefix.join("node_modules").join("@deepseek-ai").join("dsh");
        fs::create_dir_all(package.join("lib")).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"@deepseek-ai/dsh","version":"0.1.1-rc.1","license":"MIT"}"#).unwrap();
        fs::write(package.join("lib").join("bin.js"), "").unwrap();
        fs::create_dir_all(package.join("fixtures")).unwrap();
        fs::write(package.join("fixtures").join("package.json"), "{}").unwrap();
        return;
    }
    if args.iter().any(|value| value == "--help") { return; }
    if args.iter().any(|value| value == "web") {
        if !args.iter().any(|value| value == "--no-open") { std::process::exit(11); }
        let port: u16 = args[args.iter().position(|value| value == "--port").unwrap() + 1].parse().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let suppress_readiness = env::current_exe().unwrap().parent().unwrap().join("suppress-readiness").exists();
        if !suppress_readiness { println!("dsh web: http://127.0.0.1:{port}"); }
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            if attempt == 0 { thread::sleep(Duration::from_millis(1_200)); }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK");
        }
        return;
    }
    std::process::exit(2);
}
'@
$fakeNodeSourcePath = Join-Path $testRoot 'fake-node.rs'
Set-Content -LiteralPath $fakeNodeSourcePath -Value $fakeNodeSource -Encoding utf8NoBOM
$rustc = Join-Path $env:USERPROFILE '.cargo\bin\rustc.exe'
& $rustc --edition 2024 $fakeNodeSourcePath -o (Join-Path $fakeRoot 'node.exe')
if ($LASTEXITCODE -ne 0) { throw '测试假 Node 编译失败' }
$fakeArchive = Join-Path $testRoot 'release-input\node-v24.15.0-win-x64.zip'
New-Item -ItemType Directory -Path (Split-Path -Parent $fakeArchive) | Out-Null
[System.IO.Compression.ZipFile]::CreateFromDirectory((Join-Path $testRoot 'fake-node-archive'), $fakeArchive)
$fakeDigest = (Get-FileHash -LiteralPath $fakeArchive -Algorithm SHA256).Hash
Set-Content -LiteralPath (Join-Path $testLock 'package-lock.json') -Value $validLock -NoNewline
Set-Content -LiteralPath (Join-Path $testLock 'install-scripts.json') -Value '{"schema":1,"packages":[]}' -NoNewline
$runtimeOutput = Join-Path $testRoot 'runtime-out'
$built = Invoke-BuildRuntimeTest -TargetScript $isolatedScript -Arguments @('-DshVersion', '0.1.1-rc.1', '-NodeArchive', $fakeArchive, '-NodeSha256', $fakeDigest, '-OutputDirectory', $runtimeOutput)
if ($built.ExitCode -ne 0) {
    throw "小型端到端 runtime 制作应成功，实际 output=$($built.Output)"
}
$runtimeZip = Join-Path $runtimeOutput 'dsh-runtime-0.1.1-rc.1-node-24.15.0-win-x64.zip'
if (-not (Test-Path -LiteralPath $runtimeZip -PathType Leaf)) {
    throw '端到端制作必须输出固定命名 ZIP'
}
$zipEntries = [System.IO.Compression.ZipFile]::OpenRead($runtimeZip).Entries.FullName
foreach ($requiredEntry in @('inventory.json', 'THIRD_PARTY_NOTICES.json', 'app/package-lock.json', 'app/install-scripts.json')) {
    if ($requiredEntry -notin $zipEntries) {
        throw "runtime ZIP 缺少 $requiredEntry"
    }
}
if ($zipEntries -match 'smoke-home|private|npm-cache') {
    throw 'runtime ZIP 不得包含 smoke home、密钥或 npm cache'
}

# HTTP 200 只是传输层探活；官方 CLI 未发布精确 readiness 行时，runtime 不得进入发布产物。
Set-Content -LiteralPath (Join-Path $fakeRoot 'suppress-readiness') -Value '' -NoNewline
$silentArchive = Join-Path $testRoot 'silent-input\node-v24.15.0-win-x64.zip'
New-Item -ItemType Directory -Path (Split-Path -Parent $silentArchive) | Out-Null
[System.IO.Compression.ZipFile]::CreateFromDirectory((Join-Path $testRoot 'fake-node-archive'), $silentArchive)
$silentDigest = (Get-FileHash -LiteralPath $silentArchive -Algorithm SHA256).Hash
Assert-FailsWith -TargetScript $isolatedScript -Arguments @('-DshVersion', '0.1.1-rc.1', '-NodeArchive', $silentArchive, '-NodeSha256', $silentDigest, '-OutputDirectory', (Join-Path $testRoot 'silent-runtime-out')) -Pattern 'readiness|就绪'

$lockRoot = Join-Path $repositoryRoot 'runtime\locks\dsh-0.1.1-rc.1\package-lock.json'
if (Test-Path -LiteralPath $lockRoot) {
    $lock = Get-Content -Raw -LiteralPath $lockRoot | ConvertFrom-Json -AsHashtable -Depth 100
    if ($lock['packages']['']['dependencies']['@deepseek-ai/dsh'] -ne '0.1.1-rc.1') {
        throw 'lock root 必须固定 exact @deepseek-ai/dsh=0.1.1-rc.1'
    }
    if ($lock['packages']['node_modules/@deepseek-ai/dsh']['version'] -ne '0.1.1-rc.1') {
        throw 'lock 安装条目必须解析到 0.1.1-rc.1'
    }
}

Write-Output 'build-runtime contract tests passed'
