# DSH 0.1.1-rc.2 Runtime Release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `ccx-dym/dsh-gui` 发布经过锁定、探活和 Ed25519 签名的 Windows x64 DSH `0.1.1-rc.2` runtime，并维护稳定通道清单。

**Architecture:** 以提交到仓库的 exact npm lock 作为唯一依赖输入，在 GitHub Windows runner 上组合固定 Node 24.15.0 与 DSH 生产依赖。版本化 Release 资产不可覆盖；通过环境审批的发布 job 更新仓库内稳定清单和 detached signature。

**Tech Stack:** npm lockfile v3、Node.js 24.15.0、PowerShell 7、GitHub Actions、Ed25519、现有 runtime builder/smoke scripts

**Spec:** `docs/superpowers/specs/2026-08-22-dual-track-updates-first-install-design.md`

## Global Constraints

- 首发版本固定为 `@deepseek-ai/dsh@0.1.1-rc.2`，不得使用 `latest`、`npx` 或源码构建。
- runtime 必须包含 Node 24 Windows x64，不依赖用户系统 Node.js。
- 私钥仅进入 GitHub environment secret；仓库只保存公钥。
- runtime ZIP、manifest 和 signature 版本化发布，ZIP 不得原地覆盖。
- 不删除现有 `rc.1` lock、runtime 制品或用户数据。

---

### Task 1: 提交并审查 rc.2 精确依赖闭包

**Files:**
- Create: `runtime/locks/dsh-0.1.1-rc.2/package.json`
- Create: `runtime/locks/dsh-0.1.1-rc.2/package-lock.json`
- Create: `runtime/locks/dsh-0.1.1-rc.2/install-scripts.json`
- Create: `scripts/review-runtime-lock.mjs`
- Modify: `scripts/tests/runtime-lock.Tests.mjs`

**Interfaces:**
- Consumes: npm registry exact package `@deepseek-ai/dsh@0.1.1-rc.2`.
- Produces: `runtime/locks/dsh-0.1.1-rc.2`，供 `scripts/build-runtime.ps1 -DshVersion 0.1.1-rc.2` 使用。

- [ ] **Step 1: 写入失败测试**

把现有单版本读取改成显式版本循环，并让原有闭包、peer 和 install-script 断言都在循环体内执行：

```js
for (const version of ["0.1.1-rc.1", "0.1.1-rc.2"]) {
  const lockPath = path.resolve(import.meta.dirname, `../../runtime/locks/dsh-${version}/package-lock.json`);
  const lock = JSON.parse(await readFile(lockPath, "utf8"));
  const allowlist = JSON.parse(await readFile(path.join(path.dirname(lockPath), "install-scripts.json"), "utf8"));
  assert.equal(lock.packages[""].dependencies["@deepseek-ai/dsh"], version);
  assert.equal(lock.packages["node_modules/@deepseek-ai/dsh"].version, version);
  const actual = Object.entries(lock.packages)
    .filter(([, metadata]) => metadata.hasInstallScript === true)
    .map(([packagePath, metadata]) => ({
      path: packagePath,
      name: metadata.name ?? packagePath.slice(packagePath.lastIndexOf("node_modules/") + 13),
      version: metadata.version,
      integrity: metadata.integrity,
    }))
    .sort((left, right) => left.path.localeCompare(right.path));
  const approved = [...allowlist.packages].sort((left, right) => left.path.localeCompare(right.path));
  assert.deepEqual(actual, approved);
}
```

- [ ] **Step 2: 验证测试因 lock 缺失而失败**

Run: `node --test scripts/tests/runtime-lock.Tests.mjs`

Expected: FAIL，错误包含 `dsh-0.1.1-rc.2` 或 `ENOENT`。

- [ ] **Step 3: 生成 exact lock 并建立审核清单**

先创建以下 `package.json`：

```json
{
  "name": "dsh-runtime-lock-0.1.1-rc.2",
  "version": "0.0.0",
  "private": true,
  "dependencies": { "@deepseek-ai/dsh": "0.1.1-rc.2" }
}
```

随后执行：

```powershell
Push-Location runtime/locks/dsh-0.1.1-rc.2
npm install --package-lock-only --ignore-scripts --no-audit --no-fund
Pop-Location
node scripts/review-runtime-lock.mjs 0.1.1-rc.2
```

`review-runtime-lock.mjs` 必须只读 lock、输出 `hasInstallScript=true` 的 path/name/version/integrity，并由实施者逐项审查后把完全相同的数组写入 `install-scripts.json`。确认顶层 DSH record 的 `resolved` 为官方 registry tarball且 `integrity` 为 canonical `sha512-...`。

- [ ] **Step 4: 运行 lock 和构建输入门禁**

Run: `node --test scripts/tests/runtime-lock.Tests.mjs`

Run: `pwsh -NoProfile -File scripts/tests/build-runtime.Tests.ps1`

Expected: 两条命令均 PASS。

- [ ] **Step 5: 提交**

```powershell
git add runtime/locks/dsh-0.1.1-rc.2 scripts/tests/runtime-lock.Tests.mjs scripts/review-runtime-lock.mjs
git commit -m "build: lock DSH 0.1.1-rc.2 runtime"
```

---

### Task 2: 生成可复现的 runtime 发布元数据

**Files:**
- Create: `scripts/create-runtime-manifest.mjs`
- Create: `scripts/tests/create-runtime-manifest.Tests.mjs`
- Modify: `runtime/manifest.schema.json`
- Modify: `docs/runtime-release.md`

**Interfaces:**
- Consumes: `{ zipPath, dshVersion, nodeVersion, minimumDesktopVersion, artifactUrl, verifiedAt, compatibilitySummary }`.
- Produces: `createRuntimeManifest(input): Promise<RuntimeManifestV1>` 和 canonical UTF-8 JSON 文件。

- [ ] **Step 1: 写入 manifest 生成器失败测试**

```js
test("manifest 从真实 ZIP 计算 size 与 sha256", async () => {
  const manifest = await createRuntimeManifest({
    zipPath: fixture,
    dshVersion: "0.1.1-rc.2",
    nodeVersion: "24.15.0",
    minimumDesktopVersion: "0.1.0",
    artifactUrl: "https://github.com/ccx-dym/dsh-gui/releases/download/dsh-v0.1.1-rc.2-windows/dsh-runtime-0.1.1-rc.2-node-24.15.0-win-x64.zip",
    verifiedAt: "2026-08-22T00:00:00Z",
    compatibilitySummary: "Windows 10/11 x64 核心兼容验证通过；皮肤未验证时自动关闭。"
  });
  assert.equal(manifest.artifact.size, 7);
  assert.match(manifest.artifact.sha256, /^[0-9a-f]{64}$/);
});
```

- [ ] **Step 2: 验证模块尚不存在**

Run: `node --test scripts/tests/create-runtime-manifest.Tests.mjs`

Expected: FAIL，错误为无法导入 `create-runtime-manifest.mjs`。

- [ ] **Step 3: 实现严格输入验证与 canonical 输出**

实现并导出：

```js
export async function createRuntimeManifest(input) {
  assertExactSemver(input.dshVersion, "dshVersion");
  assertExactSemver(input.nodeVersion, "nodeVersion");
  assertExactSemver(input.minimumDesktopVersion, "minimumDesktopVersion");
  const artifactUrl = new URL(input.artifactUrl);
  if (artifactUrl.protocol !== "https:") throw new Error("artifactUrl 必须使用 HTTPS");
  const bytes = await readFile(input.zipPath);
  return {
    schema: 1,
    dsh_version: input.dshVersion,
    node_version: input.nodeVersion,
    minimum_desktop_version: input.minimumDesktopVersion,
    platform: "windows",
    arch: "x86_64",
    artifact: { url: artifactUrl.href, size: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex") },
    verified_at: input.verifiedAt,
    compatibility_summary: input.compatibilitySummary
  };
}
```

CLI 必须通过显式参数接收输入并以 `JSON.stringify(manifest) + "\n"` 写出；不得接受远程摘要或手填 size。

- [ ] **Step 4: 运行元数据与签名测试**

Run: `node --test scripts/tests/create-runtime-manifest.Tests.mjs scripts/tests/sign-runtime.Tests.mjs`

Expected: PASS，且非 HTTPS、非 exact semver、超长摘要均被拒绝。

- [ ] **Step 5: 提交**

```powershell
git add scripts/create-runtime-manifest.mjs scripts/tests/create-runtime-manifest.Tests.mjs runtime/manifest.schema.json docs/runtime-release.md
git commit -m "build: generate signed runtime manifests"
```

---

### Task 3: 建立人工审批的 runtime 发布工作流

**Files:**
- Create: `.github/workflows/build-runtime.yml`
- Create: `scripts/tests/workflow-policy.Tests.mjs`
- Create: `releases/runtime/stable/README.md`
- Modify: `.gitignore`

**Interfaces:**
- Consumes: workflow input `dsh_version=0.1.1-rc.2`、environment secrets `DSH_RUNTIME_SIGNING_KEY` 与 `DSH_RUNTIME_SIGNING_KEY_PASSWORD`。
- Produces: GitHub Release tag `dsh-v0.1.1-rc.2-windows` 及稳定通道更新 commit。

- [ ] **Step 1: 写工作流策略失败测试**

```js
test("runtime 发布仅在审批环境中获得 contents:write", async () => {
  const workflow = await loadWorkflow(".github/workflows/build-runtime.yml");
  assert.equal(workflow.permissions.contents, "read");
  assert.equal(workflow.jobs.publish.environment, "runtime-release");
  assert.equal(workflow.jobs.publish.permissions.contents, "write");
  assert.equal(workflow.jobs.build.permissions.contents, "read");
});
```

- [ ] **Step 2: 验证工作流缺失**

Run: `node --test scripts/tests/workflow-policy.Tests.mjs`

Expected: FAIL，错误指出 `build-runtime.yml` 不存在。

- [ ] **Step 3: 实现 build 与 publish 两段 job**

工作流固定如下入口和权限边界：

```yaml
name: Build compatible DSH runtime
on:
  workflow_dispatch:
    inputs:
      dsh_version:
        description: Exact reviewed DSH semver
        required: true
        default: 0.1.1-rc.2
permissions:
  contents: read
jobs:
  build:
    runs-on: windows-2025
    timeout-minutes: 45
  publish:
    needs: build
    runs-on: windows-2025
    environment: runtime-release
    permissions:
      contents: write
```

`build` 下载 `node-v24.15.0-win-x64.zip` 和同目录 `SHASUMS256.txt`，核对其中该文件的 SHA-256，调用现有 builder 与 smoke，将 ZIP、inventory、notices 和未签名 manifest 作为 Actions artifact 交给 `publish`。`publish` 用 secret 临时写私钥、调用 `scripts/sign-runtime.mjs`、创建不可变 Release；若 tag 已存在则失败。稳定 manifest/signature 通过专用分支提交并创建 PR，不绕过主分支保护。

- [ ] **Step 4: 验证 YAML、安全边界和本地完整门禁**

Run: `node --test scripts/tests/workflow-policy.Tests.mjs`

Run: `pnpm check`

Expected: 全部 PASS；日志步骤不回显 signing secret，workflow 顶层没有 `contents: write`。

- [ ] **Step 5: 提交**

```powershell
git add .github/workflows/build-runtime.yml scripts/tests/workflow-policy.Tests.mjs releases/runtime/stable/README.md .gitignore
git commit -m "ci: publish approved DSH runtimes"
```

---

### Task 4: 发布 rc.2 并固定稳定通道

**Files:**
- Create: `releases/runtime/stable/manifest.json`
- Create: `releases/runtime/stable/manifest.sig`
- Create: `scripts/verify-runtime-manifest.mjs`
- Modify: `docs/runtime-release.md`

**Interfaces:**
- Consumes: GitHub environment `runtime-release` 的审批与 secrets。
- Produces: 客户端可读取的 raw GitHub 固定地址和不可变 rc.2 Release URL。

- [ ] **Step 1: 配置一次性发布前提**

在 GitHub 仓库创建 `runtime-release` environment，启用 required reviewer；设置 `DSH_RUNTIME_SIGNING_KEY` 和可选 `DSH_RUNTIME_SIGNING_KEY_PASSWORD`。把对应 32-byte Ed25519 public key 以 64 位小写 hex 保存为 repository variable `DSH_RUNTIME_PUBLIC_KEY_HEX`。

- [ ] **Step 2: 手动运行 rc.2 workflow 并审批 publish job**

Run in GitHub UI: `Build compatible DSH runtime` → `Run workflow` → `dsh_version=0.1.1-rc.2`。

Expected: build 门禁 PASS，审批前不发布；审批后 Release 含 ZIP、`manifest.json`、`manifest.sig`。

- [ ] **Step 3: 审查并合并稳定通道 PR**

核对 manifest 的 URL、实际 size、SHA-256、`minimum_desktop_version=0.1.0`、签名及 Release tag 后合并自动创建的 PR。

- [ ] **Step 4: 从公开 URL 做干净下载验证**

```powershell
$manifestUrl = 'https://raw.githubusercontent.com/ccx-dym/dsh-gui/main/releases/runtime/stable/manifest.json'
$signatureUrl = 'https://raw.githubusercontent.com/ccx-dym/dsh-gui/main/releases/runtime/stable/manifest.sig'
Invoke-WebRequest -Uri $manifestUrl -OutFile "$env:TEMP\dsh-manifest.json"
Invoke-WebRequest -Uri $signatureUrl -OutFile "$env:TEMP\dsh-manifest.sig"
node scripts/verify-runtime-manifest.mjs "$env:TEMP\dsh-manifest.json" "$env:TEMP\dsh-manifest.sig" "$env:DSH_RUNTIME_PUBLIC_KEY_HEX"
```

Expected: 输出 `verified dsh 0.1.1-rc.2 windows-x86_64`，无敏感值。

- [ ] **Step 5: 提交发布说明修订**

```powershell
git add docs/runtime-release.md releases/runtime/stable/manifest.json releases/runtime/stable/manifest.sig scripts/verify-runtime-manifest.mjs
git commit -m "docs: record DSH 0.1.1-rc.2 runtime release"
```
