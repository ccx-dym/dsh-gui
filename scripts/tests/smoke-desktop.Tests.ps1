$ErrorActionPreference = 'Stop'

$scriptPath = (Resolve-Path (Join-Path $PSScriptRoot '..\smoke-desktop.ps1')).Path
$environmentKeys = @(
    'DSH_DESKTOP_NPM_REGISTRY_ROOT',
    'DSH_DESKTOP_COMPAT_MANIFEST_URL',
    'DSH_DESKTOP_COMPAT_SIGNATURE_URL',
    'DSH_DESKTOP_COMPAT_PUBLIC_KEY'
)

# 使用独立 PowerShell 进程，确保测试不会继承开发机偶然存在的发布通道配置。
$startInfo = [System.Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = (Get-Command pwsh).Source
$startInfo.ArgumentList.Add('-NoProfile')
$startInfo.ArgumentList.Add('-File')
$startInfo.ArgumentList.Add($scriptPath)
$startInfo.ArgumentList.Add('-RequireReleaseChannel')
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
$startInfo.UseShellExecute = $false
foreach ($key in $environmentKeys) {
    $null = $startInfo.Environment.Remove($key)
}
# 旧脚本会先检查开发工具；清空 PATH 可让缺少发布配置的错误顺序测试快速暴露。
$startInfo.Environment['PATH'] = ''

$process = [System.Diagnostics.Process]::Start($startInfo)
$stdout = $process.StandardOutput.ReadToEnd()
$stderr = $process.StandardError.ReadToEnd()
$process.WaitForExit()
$combined = "$stdout`n$stderr"

if ($process.ExitCode -eq 0) {
    throw '缺少发布通道配置时，RequireReleaseChannel 不得通过'
}
if ($combined -notmatch 'release_channel_missing') {
    throw "缺少发布通道配置时必须返回稳定错误码 release_channel_missing，实际输出: $combined"
}

Write-Host 'smoke-desktop release channel gate test passed'

# 配置齐全时必须越过发布通道门禁；空 PATH 让测试在后续工具检查处快速、确定地停止。
$configuredInfo = [System.Diagnostics.ProcessStartInfo]::new()
$configuredInfo.FileName = (Get-Command pwsh).Source
$configuredInfo.ArgumentList.Add('-NoProfile')
$configuredInfo.ArgumentList.Add('-File')
$configuredInfo.ArgumentList.Add($scriptPath)
$configuredInfo.ArgumentList.Add('-RequireReleaseChannel')
$configuredInfo.RedirectStandardOutput = $true
$configuredInfo.RedirectStandardError = $true
$configuredInfo.UseShellExecute = $false
$configuredInfo.Environment['PATH'] = ''
$configuredInfo.Environment['DSH_DESKTOP_NPM_REGISTRY_ROOT'] = 'https://registry.npmjs.org/'
$configuredInfo.Environment['DSH_DESKTOP_COMPAT_MANIFEST_URL'] = 'https://example.test/manifest.json'
$configuredInfo.Environment['DSH_DESKTOP_COMPAT_SIGNATURE_URL'] = 'https://example.test/manifest.sig'
$configuredInfo.Environment['DSH_DESKTOP_COMPAT_PUBLIC_KEY'] = ('a' * 64)

$configuredProcess = [System.Diagnostics.Process]::Start($configuredInfo)
$configuredStdout = $configuredProcess.StandardOutput.ReadToEnd()
$configuredStderr = $configuredProcess.StandardError.ReadToEnd()
$configuredProcess.WaitForExit()
$configuredCombined = "$configuredStdout`n$configuredStderr"

if ($configuredCombined -match 'release_channel_missing') {
    throw "配置齐全时不应触发发布通道缺失错误: $configuredCombined"
}
if ($configuredCombined -notmatch '缺少开发命令: node') {
    throw "配置齐全时应继续执行后续开发环境门禁，实际输出: $configuredCombined"
}

Write-Host 'smoke-desktop configured channel continuation test passed'
