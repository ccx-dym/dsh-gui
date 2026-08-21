$ErrorActionPreference = 'Stop'

# 这里只检查开发环境和阶段 1 自动化门禁，不修改注册表或用户数据。
# 先验证命令，避免门禁执行到中途才暴露工具链缺口。
foreach ($commandName in @('node', 'pnpm', 'cargo')) {
    if (-not (Get-Command $commandName -ErrorAction SilentlyContinue)) {
        throw "缺少开发命令: $commandName"
    }
}

$webViewClientId = '{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
# WebView2 可能按当前用户或整机安装，因此同时检查两个官方客户端注册位置。
$webViewKeys = @(
    "HKCU:\Software\Microsoft\EdgeUpdate\Clients\$webViewClientId",
    "HKLM:\Software\WOW6432Node\Microsoft\EdgeUpdate\Clients\$webViewClientId"
)
if (-not ($webViewKeys | Where-Object { Test-Path -LiteralPath $_ })) {
    throw '未检测到 Microsoft Edge WebView2 Runtime'
}

pnpm check
if ($LASTEXITCODE -ne 0) {
    throw 'pnpm check 未通过'
}

Write-Host '自动化门禁已通过。请运行 pnpm tauri dev，并依次手工检查：'
Write-Host '1. 启动后模拟 DSH 就绪，主窗口显示本地 WebUI。'
Write-Host '2. 点击窗口关闭按钮后隐藏到系统托盘，后台服务继续运行。'
Write-Host '3. 从托盘打开 DSH，窗口恢复并获得焦点。'
Write-Host '4. 再次运行 pnpm tauri dev，不产生第二个桌面实例或第二份服务。'
Write-Host '5. 从托盘退出，桌面进程和模拟 DSH 子进程树均被回收。'
