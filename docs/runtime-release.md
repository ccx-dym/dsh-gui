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

执行 `npm install --package-lock-only --ignore-scripts --no-audit --no-fund --legacy-peer-deps`，
避免 npm 自动扩张 peer 图；同一选项会固定用于发布的 `npm ci`。然后人工检查：

- lock root 与 `node_modules/@deepseek-ai/dsh` 都是目标 exact version；
- registry、integrity、可选平台包和 install scripts；
- 完整传递依赖的许可证。MIT 标识并不自动覆盖所有传递依赖，发布者仍需复核生成的
  `THIRD_PARTY_NOTICES.json`，并补充许可证正文或其他法务要求。

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

去掉 `-WhatIf` 才会创建固定 staging、运行 `npm ci --omit=dev`、核对已安装包、执行
CLI help 与短时回环 Web smoke，最后生成 `inventory.json`、
`THIRD_PARTY_NOTICES.json` 和 ZIP。输出目录必须为空，避免静默覆盖历史制品。

发布前必须人工复核清单、notices、压缩包内容，并确认其中没有 npm cache、用户目录、
环境文件或密钥。将 ZIP 上传到受控 HTTPS 下载地址后，以实际 URL、大小和 SHA-256
填写与 `runtime/manifest.schema.json` 一致的兼容清单。

## 独立签名

生产 Ed25519 私钥只能位于仓库外部，由发布环境通过变量传入：

```powershell
$env:DSH_RUNTIME_SIGNING_KEY_FILE = 'X:\secure\runtime-ed25519-private.pem'
node scripts/sign-runtime.mjs runtime-out\manifest.json runtime-out\manifest.sig
```

签名覆盖清单文件的原始 bytes，输出为 128 个小写 hex 字符的 detached signature。
脚本不会打印私钥路径、内容或签名。测试所用临时 Ed25519 seed/PEM 只能用于闭环测试，
不得当作生产密钥。
