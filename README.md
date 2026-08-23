# DSH Desktop

面向 Windows 10/11 x64 的 DeepSeek Harness 桌面客户端。它使用 Tauri 2 与系统
WebView2，在独立窗口中运行官方 DSH WebUI，并提供当前用户安装、系统托盘、DSH/客户端
双通道更新和自定义沉浸式图片皮肤。

## 快速开始

1. 从 [Releases](https://github.com/ccx-dym/dsh-gui/releases) 下载最新
   `DSH-Desktop_<版本>_x64-setup.exe`。
2. 安装并打开 DSH Desktop；不需要管理员权限，也不需要下载 DSH 源码。
3. 在窗口右侧点击“检查兼容版本”或“重新检查”，按提示安装签名验证的 DSH runtime。
4. 重启客户端后进入 DeepSeek Harness 官方 WebUI。

完整的安装、更新、托盘、皮肤、数据目录和故障排查说明见
[中文使用指南](docs/用户使用指南.md)。开发、测试与发布说明见
[开发说明](docs/development.md)。

## 更新模型

- DSH Desktop 扫描 DeepSeek Harness 官方版本，但只安装已通过兼容验证的签名 runtime。
- DSH runtime 和桌面客户端独立更新；新 DSH 若需要新的兼容逻辑，客户端会先提示更新
  DSH Desktop。
- 更新失败时保持当前 runtime 与数据不变；不自动覆盖全局 DSH 或 `~/.dsh`。

## 当前定位

这是个人/小范围测试版本。项目保留官方 DSH 功能与 WebUI，不是 DeepSeek 官方发布物。
发现问题时请附上版本、复现步骤和脱敏后的诊断阶段，不要提交 API Key 或聊天内容。
