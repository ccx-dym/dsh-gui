# DSH runtime stable channel

此目录由 `Build compatible DSH runtime` 工作流通过专用分支和 Pull Request 更新，
不允许发布 job 直接写入默认分支。

首次发布并合并稳定通道 PR 后，桌面端读取：

- `manifest.json`：runtime 版本、兼容性、不可变 Release URL、大小和 SHA-256。
- `manifest.sig`：对 `manifest.json` 原始 UTF-8 bytes 的 Ed25519 detached signature。

版本化 ZIP、清单和签名保存在 `dsh-v<version>-windows` GitHub Release 中；已存在的
tag 不得覆盖。私钥仅允许保存在受人工审批保护的 `runtime-release` environment secret，
不得写入仓库、Actions artifact 或日志。

发布恢复只允许对同一次 workflow run 使用 GitHub 的 failed-job rerun，以复用同一份
Actions artifact。artifact 过期后不得重新 dispatch 并覆盖旧分支；必须先人工审查残留
Pull Request 和 `automation/runtime-<version>-stable` 分支，关闭或处置后再发起新 dispatch，
且禁止 force push。

当前 runtime ZIP 不承诺 bit-for-bit reproducible；稳定通道的可重入判断仅接受同一 run
保存的候选 manifest/signature 原始 bytes，不把重新构建的 ZIP 视为相同制品。

发布前还需运行固定版本的 workflow 静态门禁：

```powershell
go run github.com/rhysd/actionlint/cmd/actionlint@v1.7.7 .github/workflows/build-runtime.yml
```
