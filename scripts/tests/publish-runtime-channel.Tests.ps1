$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path
$scriptPath = Join-Path $repositoryRoot 'scripts\publish-runtime-channel.ps1'
$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) "dsh-channel-test-$([guid]::NewGuid().ToString('N'))"
$remotePath = Join-Path $testRoot 'remote.git'
$workPath = Join-Path $testRoot 'work'
$candidatePath = Join-Path $testRoot 'candidate'
$fakeBin = Join-Path $testRoot 'fake-bin'
$fakeLog = Join-Path $testRoot 'fake-gh.log'
New-Item -ItemType Directory -Path $testRoot, $workPath, $candidatePath, $fakeBin | Out-Null

git init --bare $remotePath | Out-Null
git -C $workPath init -b main | Out-Null
git -C $workPath config user.name 'Runtime Test'
git -C $workPath config user.email 'runtime-test@example.invalid'
$stablePath = Join-Path $workPath 'releases\runtime\stable'
New-Item -ItemType Directory -Path $stablePath | Out-Null
Set-Content -LiteralPath (Join-Path $stablePath 'README.md') -Value 'stable channel' -Encoding utf8NoBOM
git -C $workPath add releases/runtime/stable/README.md
git -C $workPath commit -m 'seed default branch' | Out-Null
$sourceSha = (git -C $workPath rev-parse HEAD).Trim()
git -C $workPath remote add origin $remotePath
git -C $workPath push -u origin main | Out-Null

$manifestPath = Join-Path $candidatePath 'manifest.json'
$signaturePath = Join-Path $candidatePath 'manifest.sig'
[System.IO.File]::WriteAllBytes($manifestPath, [System.Text.Encoding]::UTF8.GetBytes("{`"schema`":1}`n"))
[System.IO.File]::WriteAllBytes($signaturePath, [System.Text.Encoding]::ASCII.GetBytes(('ab' * 64)))

$fakeGh = @'
param([Parameter(ValueFromRemainingArguments = $true)][string[]] $Remaining)
Add-Content -LiteralPath $env:FAKE_GH_LOG -Value ($Remaining -join ' ')
if ($Remaining.Count -ge 2 -and $Remaining[0] -eq 'pr' -and $Remaining[1] -eq 'list') {
    if ([string]::IsNullOrWhiteSpace($env:FAKE_PR_NUMBER)) { Write-Output '[]' }
    else {
        $base = if ([string]::IsNullOrWhiteSpace($env:FAKE_PR_BASE)) { 'main' } else { $env:FAKE_PR_BASE }
        $head = if ([string]::IsNullOrWhiteSpace($env:FAKE_PR_HEAD)) { 'automation/runtime-0.1.1-rc.2-stable' } else { $env:FAKE_PR_HEAD }
        Write-Output "[{`"number`":$env:FAKE_PR_NUMBER,`"baseRefName`":`"$base`",`"headRefName`":`"$head`"}]"
    }
    exit 0
}
if ($Remaining.Count -ge 2 -and $Remaining[0] -eq 'pr' -and $Remaining[1] -eq 'create') {
    Write-Output 'https://example.invalid/pull/17'
    exit 0
}
exit 9
'@
Set-Content -LiteralPath (Join-Path $fakeBin 'gh.ps1') -Value $fakeGh -Encoding utf8NoBOM
$env:FAKE_GH_LOG = $fakeLog
$env:PATH = "$fakeBin$([System.IO.Path]::PathSeparator)$env:PATH"
$branch = 'automation/runtime-0.1.1-rc.2-stable'
$commonArguments = @{
    SourceSha = $sourceSha
    DefaultBranch = 'main'
    Branch = $branch
    CandidateManifest = $manifestPath
    CandidateSignature = $signaturePath
    StableDirectory = 'releases/runtime/stable'
}

Push-Location $workPath
try {
    & $scriptPath @commonArguments
    $remoteHead = (git --git-dir $remotePath rev-parse "refs/heads/$branch").Trim()
    if ((git --git-dir $remotePath rev-parse "$remoteHead^").Trim() -cne $sourceSha) {
        throw '新稳定分支必须直接基于本次 source SHA'
    }
    $remoteManifest = (git --git-dir $remotePath rev-parse "$remoteHead`:releases/runtime/stable/manifest.json").Trim()
    if ($remoteManifest -cne (git hash-object $manifestPath).Trim()) {
        throw '远端 manifest 必须与候选 bytes 完全一致'
    }
    $firstLog = @(Get-Content -LiteralPath $fakeLog)
    if (@($firstLog | Where-Object { $_ -match '^pr create ' }).Count -ne 1) {
        throw '首次运行必须创建一条 PR'
    }

    # 重跑使用相同候选时复用已存在分支与 PR，不产生第二个 commit 或 PR。
    $env:FAKE_PR_NUMBER = '17'
    & $scriptPath @commonArguments
    if ((git --git-dir $remotePath rev-parse "refs/heads/$branch").Trim() -cne $remoteHead) {
        throw '完全相同的重跑不得改变远端稳定分支'
    }
    $secondLog = @(Get-Content -LiteralPath $fakeLog)
    if (@($secondLog | Where-Object { $_ -match '^pr create ' }).Count -ne 1) {
        throw '已存在开放 PR 时不得再次创建'
    }

    # 同名开放 PR 若目标分支不一致，不能被误当成本次稳定通道 PR。
    $env:FAKE_PR_BASE = 'unexpected-base'
    $wrongBaseRejected = $false
    try { & $scriptPath @commonArguments } catch { $wrongBaseRejected = $true }
    if (-not $wrongBaseRejected) { throw '错误 baseRefName 的开放 PR 必须被拒绝' }
    $env:FAKE_PR_BASE = $null

    # 仅 bytes 相同仍不够：已有稳定分支必须直接基于本轮 source SHA。
    git switch --detach $sourceSha | Out-Null
    git commit --allow-empty -m 'alternate source fixture' | Out-Null
    $alternateSource = (git rev-parse HEAD).Trim()
    $alternateArguments = $commonArguments.Clone()
    $alternateArguments.SourceSha = $alternateSource
    $ancestryRejected = $false
    try { & $scriptPath @alternateArguments } catch { $ancestryRejected = $true }
    if (-not $ancestryRejected) { throw '已有稳定分支的 base ancestry 不同必须被拒绝' }

    # 相同版本的候选 bytes 漂移必须失败关闭，不能覆盖远端分支。
    [System.IO.File]::WriteAllBytes($manifestPath, [System.Text.Encoding]::UTF8.GetBytes("{`"schema`":2}`n"))
    $driftRejected = $false
    try { & $scriptPath @commonArguments } catch { $driftRejected = $true }
    if (-not $driftRejected) { throw '候选 bytes 漂移必须被拒绝' }
    if ((git --git-dir $remotePath rev-parse "refs/heads/$branch").Trim() -cne $remoteHead) {
        throw '拒绝漂移后远端稳定分支不得变化'
    }

    # native git 故障必须中止，不能沿用上一次探测出的陈旧分支状态。
    [System.IO.File]::WriteAllBytes($manifestPath, [System.Text.Encoding]::UTF8.GetBytes("{`"schema`":1}`n"))
    git remote set-url origin (Join-Path $testRoot 'missing-remote.git')
    $nativeFailureRejected = $false
    try { & $scriptPath @commonArguments } catch { $nativeFailureRejected = $true }
    if (-not $nativeFailureRejected) { throw 'native git 失败必须终止 channel 发布'
    }
}
finally {
    Pop-Location
}

Write-Output "publish runtime channel behavior tests passed; fixture retained: $testRoot"
