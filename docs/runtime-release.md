# 兼容 DSH runtime 发布

DSH Desktop 的 runtime 是独立于桌面安装包的版本化 ZIP。发布输入必须是已审查并提交的
`runtime/locks/dsh-<version>/package-lock.json`、本地官方 Node Windows x64 ZIP 及其官方
SHA-256。制作阶段只执行 `npm ci`，不会用 `latest`、`npx`、全局安装或重新解析依赖。

## 首次审查一个 DSH 版本

在独立目录创建仅含 exact 依赖的 `package.json`：

```json
{
  "private": true,
  "dependencies": { "@deepseek-ai/dsh": "0.1.1-rc.1" }
}
```

执行 `npm install --package-lock-only --ignore-scripts --no-audit --no-fund`，使用 npm
标准 peer 解析生成完整运行闭包。禁止 `--legacy-peer-deps`：它会跳过 DSH app-boot 等包的
必需 peer，使 CLI 在启动时出现 `ERR_MODULE_NOT_FOUND`。然后人工检查：

- lock root 与 `node_modules/@deepseek-ai/dsh` 都是目标 exact version；
- registry、integrity、可选平台包和 install scripts；
- 完整传递依赖的许可证。MIT 标识并不自动覆盖所有传递依赖，发布者仍需复核生成的
  `THIRD_PARTY_NOTICES.json`，并补充许可证正文或其他法务要求。

不能用 `--ignore-scripts`：`node-pty`、`koffi` 和 DSH subprocess 等组件依赖安装脚本
保留完整功能。每次 lock 变更都必须同时审查 `hasInstallScript=true` 的完整集合，并将
package path、name、version、integrity 精确记录到 `install-scripts.json`。打包脚本会在
`npm ci` 前严格比较两个集合；任何新增、删除、版本或 integrity 变化都会拒绝发布。
这是供应链执行边界，发布 runner 必须隔离、无长期凭据，并预装与 Node 原生模块兼容的
Visual C++ Build Tools。allowlist 只表示“已人工审查并允许执行”，不表示脚本天然安全。

lock 审查完成后才能提交。发布制作不得再次运行 `npm install`。

## 制作与验证

从 [Node.js 官方发布目录](https://nodejs.org/download/release/)下载固定版本的
`node-v<version>-win-x64.zip`，从同一目录的 `SHASUMS256.txt` 获取摘要。先 dry-run：

```powershell
pwsh -NoProfile -File scripts/build-runtime.ps1 `
  -DshVersion 0.1.1-rc.1 `
  -NodeArchive C:\release-input\node-v24.15.0-win-x64.zip `
  -NodeSha256 <64位SHA-256> `
  -OutputDirectory runtime-out `
  -WhatIf
```

去掉 `-WhatIf` 才会创建固定 staging、运行允许安装脚本的 `npm ci --omit=dev`、核对已安装包、执行
CLI help 与短时回环 Web smoke，最后生成 `inventory.json`、
`THIRD_PARTY_NOTICES.json` 和 ZIP。输出目录必须为空，避免静默覆盖历史制品。

发布前必须人工复核清单、notices、压缩包内容，并确认其中没有 npm cache、用户目录、
环境文件或密钥。ZIP 的公开地址确定后，必须用生成器从实际 ZIP bytes 计算 size 和
SHA-256；禁止手填或从远程元数据复制这两个字段：

```powershell
node scripts/create-runtime-manifest.mjs `
  --zip runtime-out\dsh-runtime-0.1.1-rc.2-node-24.15.0-win-x64.zip `
  --dsh-version 0.1.1-rc.2 `
  --node-version 24.15.0 `
  --minimum-desktop-version 0.1.0 `
  --artifact-url https://github.com/ccx-dym/dsh-gui/releases/download/dsh-v0.1.1-rc.2-windows/dsh-runtime-0.1.1-rc.2-node-24.15.0-win-x64.zip `
  --verified-at 2026-08-22T00:00:00Z `
  --compatibility-summary 'Windows 10/11 x64 核心兼容验证通过；皮肤未验证时自动关闭。' `
  --output runtime-out\manifest.json
```

所有版本参数必须是 exact semver；`artifact-url` 必须是无凭据、查询参数和片段的 HTTPS
地址；`verified-at` 必须是有效的 `YYYY-MM-DDTHH:mm:ssZ` UTC 时间；兼容性摘要去除首尾
空白后不得为空、不得包含控制字符，并且最多 512 个 Unicode 字符。输出文件必须尚不存在，
生成器会以固定属性顺序写入紧凑 UTF-8 JSON，并只添加一个 LF。随后再人工核对生成结果与
`runtime/manifest.schema.json`，不可在签名前格式化或改写文件。

## 独立签名

生产 Ed25519 私钥只能位于仓库外部，由发布环境通过变量传入：

```powershell
$env:DSH_RUNTIME_SIGNING_KEY_FILE = 'X:\secure\runtime-ed25519-private.pem'
node scripts/sign-runtime.mjs runtime-out\manifest.json runtime-out\manifest.sig
```

签名覆盖清单文件的原始 bytes，输出为 128 个小写 hex 字符的 detached signature。
脚本不会打印私钥路径、内容或签名。测试所用临时 Ed25519 seed/PEM 只能用于闭环测试，
不得当作生产密钥。

## 桌面端发布通道配置

正式构建通过构建环境注入以下非敏感发布配置；仓库和安装包源码不硬编码 endpoint，
也不接受运行时 token：

- `DSH_DESKTOP_NPM_REGISTRY_ROOT`：官方 npm registry 的 HTTPS 根地址；
- `DSH_DESKTOP_COMPAT_MANIFEST_URL`：兼容清单 HTTPS 地址；
- `DSH_DESKTOP_COMPAT_SIGNATURE_URL`：detached signature HTTPS 地址；
- `DSH_DESKTOP_COMPAT_PUBLIC_KEY`：64 位小写 hex Ed25519 发布公钥；
- `DSH_DESKTOP_UPDATE_CHANNEL`：可选安全 channel 标识，默认 `stable`。

CI 或隔离发布终端可直接按下面方式注入；值均来自已发布并人工复核的 release，不能使用
示例占位符生成对外安装包：

```powershell
$env:DSH_DESKTOP_NPM_REGISTRY_ROOT = 'https://registry.npmjs.org/'
$env:DSH_DESKTOP_COMPAT_MANIFEST_URL = 'https://<受控发布域>/stable/manifest.json'
$env:DSH_DESKTOP_COMPAT_SIGNATURE_URL = 'https://<受控发布域>/stable/manifest.sig'
$env:DSH_DESKTOP_COMPAT_PUBLIC_KEY = '<64位小写hex发布公钥>'
$env:DSH_DESKTOP_UPDATE_CHANNEL = 'stable'
pnpm tauri build
```

任一必填项缺失或无效时，桌面端进入明确的“发布通道尚未配置”状态，不联网、不崩溃、
不提供安装按钮。在线阶段只下载、复核并封存 runtime，再写入不含 URL 的 pending 记录；
只有用户二次确认且应用下次冷启动时，才会在 supervisor 启动前执行恢复/隔离探活/激活。
