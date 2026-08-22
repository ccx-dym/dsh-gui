[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidatePattern('^[0-9a-f]{40}$')]
    [string] $SourceSha,

    [Parameter(Mandatory)]
    [ValidatePattern('^[A-Za-z0-9._-]+$')]
    [string] $DefaultBranch,

    [Parameter(Mandatory)]
    [ValidatePattern('^automation/runtime-[0-9A-Za-z.-]+-stable$')]
    [string] $Branch,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $CandidateManifest,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $CandidateSignature,

    [Parameter(Mandatory)]
    [ValidateScript({ $_ -ceq 'releases/runtime/stable' })]
    [string] $StableDirectory
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
Set-StrictMode -Version Latest

$manifestPath = [System.IO.Path]::GetFullPath($CandidateManifest)
$signaturePath = [System.IO.Path]::GetFullPath($CandidateSignature)
foreach ($candidate in @($manifestPath, $signaturePath)) {
    if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
        throw 'stable channel candidate file is missing'
    }
}

# source commit 必须已由 checkout 固定在本地对象库中，避免远端分支探测失败时沿用陈旧 SHA。
git cat-file -e "$SourceSha^{commit}"
$remoteRows = @(git ls-remote --heads origin "refs/heads/$Branch")
if ($remoteRows.Count -gt 1) {
    throw 'stable channel remote branch lookup is ambiguous'
}
$remoteExists = $remoteRows.Count -eq 1
$remoteRef = "refs/remotes/origin/$Branch"
$stableManifest = "$StableDirectory/manifest.json"
$stableSignature = "$StableDirectory/manifest.sig"

if (-not $remoteExists) {
    # 版本稳定分支只允许从本轮已审核 source SHA 创建一次；绝不 force push 或覆盖。
    git switch --detach $SourceSha | Out-Null
    git switch -c $Branch | Out-Null
    Copy-Item -LiteralPath $manifestPath -Destination $stableManifest
    Copy-Item -LiteralPath $signaturePath -Destination $stableSignature
    git config user.name 'github-actions[bot]'
    git config user.email '41898282+github-actions[bot]@users.noreply.github.com'
    git add -- $stableManifest $stableSignature
    git commit -m "release: promote $Branch" | Out-Null
    git push origin "HEAD:refs/heads/$Branch" | Out-Null
}
else {
    # 重跑只能复用“直接基于相同 source、且最终 diff 仅为相同 bytes”的版本分支。
    git fetch --no-tags origin "+refs/heads/$Branch`:$remoteRef" | Out-Null
    $remoteHead = (git rev-parse $remoteRef).Trim()
    $remoteParent = (git rev-parse "$remoteHead^").Trim()
    if ($remoteParent -cne $SourceSha) {
        throw 'existing stable channel branch does not directly descend from source SHA'
    }
    $changedPaths = @(git diff --name-only "$SourceSha..$remoteHead")
    $expectedPaths = @($stableManifest, $stableSignature)
    if ($changedPaths.Count -ne 2 -or
        (Compare-Object -CaseSensitive ($changedPaths | Sort-Object) ($expectedPaths | Sort-Object))) {
        throw 'existing stable channel branch changes unexpected paths'
    }
    $remoteManifestBlob = (git rev-parse "$remoteRef`:$stableManifest").Trim()
    $remoteSignatureBlob = (git rev-parse "$remoteRef`:$stableSignature").Trim()
    $candidateManifestBlob = (git hash-object -- $manifestPath).Trim()
    $candidateSignatureBlob = (git hash-object -- $signaturePath).Trim()
    if ($remoteManifestBlob -cne $candidateManifestBlob -or
        $remoteSignatureBlob -cne $candidateSignatureBlob) {
        throw 'existing stable channel bytes differ from the approved candidate'
    }
}

$pullRequests = @(
    gh pr list `
        --head $Branch `
        --state open `
        --json number,baseRefName,headRefName `
        --limit 2 | ConvertFrom-Json
)
if ($pullRequests.Count -gt 1) {
    throw 'multiple open stable channel pull requests are ambiguous'
}
if ($pullRequests.Count -eq 1 -and
    ($pullRequests[0].baseRefName -cne $DefaultBranch -or
     $pullRequests[0].headRefName -cne $Branch)) {
    throw 'existing pull request does not match the stable channel base and head'
}
if ($pullRequests.Count -eq 0) {
    gh pr create `
        --base $DefaultBranch `
        --head $Branch `
        --title "release: promote $Branch" `
        --body 'Promotes the signed runtime channel after runtime-release environment approval.' | Out-Null
    Write-Output 'stable channel branch pushed and pull request created'
}
else {
    Write-Output "stable channel pull request reused: #$($pullRequests[0].number)"
}
