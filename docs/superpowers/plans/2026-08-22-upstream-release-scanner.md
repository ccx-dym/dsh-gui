# Upstream DSH Release Scanner Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 每 12 小时扫描官方 npm 的 DSH 版本，新版本只创建去重候选并提示兼容验证，不自动授权安装。

**Architecture:** 一个无长期凭据的 Node 脚本从官方 npm registry 读取 dist-tags 和 exact version record，并与仓库 locks、runtime Releases 和已有候选 issue 比较。只读 scan job 生成候选 artifact；单独最小写权限 job 创建带结构化 marker 的 issue。

**Tech Stack:** Node.js 24、GitHub Actions、GitHub REST API、npm registry metadata

**Spec:** `docs/superpowers/specs/2026-08-22-dual-track-updates-first-install-design.md`

## Global Constraints

- npm registry 是版本事实来源；GitHub upstream 仅提供 notes/tag 审计信息。
- 每 12 小时扫描，也允许 `workflow_dispatch`。
- 发现新版不得更新 stable manifest、不得签名、不得发布 runtime。
- metadata 响应限时、限大小、仅 HTTPS；日志不输出 token/header/不可信正文。
- 同一个 exact version 只能有一个开放候选 issue。

---

### Task 1: 实现 npm 候选发现与去重纯函数

**Files:**
- Create: `scripts/scan-dsh-upstream.mjs`
- Create: `scripts/tests/scan-dsh-upstream.Tests.mjs`

**Interfaces:**
- Consumes: registry document、`knownVersions: Set<string>`、`openCandidateVersions: Set<string>`。
- Produces: `discoverCandidate(input): { status: "none" | "candidate", version?: string, integrity?: string, tarball?: string }`。

- [ ] **Step 1: 写失败测试**

```js
test("发现未锁定且未建 issue 的 exact latest", () => {
  const result = discoverCandidate({ registry: fixture("rc2.json"), knownVersions: new Set(["0.1.1-rc.1"]), openCandidateVersions: new Set() });
  assert.deepEqual(result, {
    status: "candidate",
    version: "0.1.1-rc.2",
    integrity: fixtureIntegrity,
    tarball: "https://registry.npmjs.org/@deepseek-ai/dsh/-/dsh-0.1.1-rc.2.tgz"
  });
});

test("已有候选 marker 时去重", () => {
  const result = discoverCandidate({ registry: fixture("rc2.json"), knownVersions: new Set(), openCandidateVersions: new Set(["0.1.1-rc.2"]) });
  assert.equal(result.status, "none");
});
```

- [ ] **Step 2: 验证 scanner 缺失**

Run: `node --test scripts/tests/scan-dsh-upstream.Tests.mjs`

Expected: FAIL，无法导入 scanner。

- [ ] **Step 3: 实现严格解析**

```js
export function discoverCandidate({ registry, knownVersions, openCandidateVersions }) {
  const version = registry["dist-tags"]?.latest;
  const exact = registry.versions?.[version];
  if (!EXACT_SEMVER.test(version) || exact?.version !== version) throw new Error("invalid_registry_metadata");
  if (!/^sha512-[A-Za-z0-9+/]+={0,2}$/.test(exact.dist?.integrity)) throw new Error("invalid_registry_integrity");
  const tarball = new URL(exact.dist?.tarball);
  if (tarball.protocol !== "https:" || tarball.hostname !== "registry.npmjs.org") throw new Error("invalid_registry_tarball");
  if (knownVersions.has(version) || openCandidateVersions.has(version)) return { status: "none" };
  return { status: "candidate", version, integrity: exact.dist.integrity, tarball: tarball.href };
}
```

CLI 使用 `fetch` + `AbortSignal.timeout(15000)`，在读取 body 前拒绝 `content-length > 2097152`，最终 JSON output 只含上述四个字段。

- [ ] **Step 4: 运行 parser 测试**

Run: `node --test scripts/tests/scan-dsh-upstream.Tests.mjs`

Expected: PASS；错误 host、非 HTTPS、缺 exact record、无效 integrity、超大 body 和 timeout 均为稳定错误码。

- [ ] **Step 5: 提交**

```powershell
git add scripts/scan-dsh-upstream.mjs scripts/tests/scan-dsh-upstream.Tests.mjs
git commit -m "feat: detect new official DSH releases"
```

---

### Task 2: 建立 12 小时扫描与候选 issue 工作流

**Files:**
- Create: `.github/workflows/scan-upstream.yml`
- Modify: `scripts/tests/workflow-policy.Tests.mjs`
- Create: `.github/ISSUE_TEMPLATE/dsh-runtime-candidate.yml`

**Interfaces:**
- Consumes: scanner JSON output 和 GitHub open issue markers。
- Produces: 标题 `DSH runtime candidate: <version>`、body marker `<!-- dsh-runtime-candidate:<version> -->`。

- [ ] **Step 1: 写 schedule/权限失败测试**

```js
test("scanner 每 12 小时运行且 scan job 只读", async () => {
  const workflow = await loadWorkflow(".github/workflows/scan-upstream.yml");
  assert.deepEqual(workflow.on.schedule, [{ cron: "17 */12 * * *" }]);
  assert.equal(workflow.jobs.scan.permissions.contents, "read");
  assert.equal(workflow.jobs.create_candidate.permissions.issues, "write");
  assert.equal(workflow.jobs.create_candidate.permissions.contents, "read");
});
```

- [ ] **Step 2: 验证 workflow 缺失**

Run: `node --test scripts/tests/workflow-policy.Tests.mjs`

Expected: FAIL，指出 `scan-upstream.yml` 不存在。

- [ ] **Step 3: 实现扫描与最小写权限分离**

```yaml
name: Scan official DSH releases
on:
  schedule:
    - cron: '17 */12 * * *'
  workflow_dispatch:
permissions:
  contents: read
jobs:
  scan:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
  create_candidate:
    needs: scan
    if: needs.scan.outputs.status == 'candidate'
    runs-on: ubuntu-24.04
    permissions:
      contents: read
      issues: write
```

`scan` 从 `runtime/locks/dsh-*` 读取 known versions，并通过只读 GitHub API 读取带 marker 的开放 issue。`create_candidate` 只用经 semver 正则复核后的 output 构造固定模板 issue，body 包含 version、integrity、tarball、上游 GitHub compare 链接和八项核心兼容检查清单。

- [ ] **Step 4: 运行工作流测试与本地 fixture 扫描**

Run: `node --test scripts/tests/workflow-policy.Tests.mjs scripts/tests/scan-dsh-upstream.Tests.mjs`

Expected: PASS；同一 rc.2 重跑不再创建第二个候选。

- [ ] **Step 5: 提交**

```powershell
git add .github/workflows/scan-upstream.yml .github/ISSUE_TEMPLATE/dsh-runtime-candidate.yml scripts/tests/workflow-policy.Tests.mjs
git commit -m "ci: scan upstream DSH every twelve hours"
```

---

### Task 3: 把候选状态接入客户端通知语义

**Files:**
- Modify: `src-tauri/src/update/coordinator.rs`
- Modify: `src-tauri/src/update_ui.rs`
- Modify: `src/runtime-events.ts`
- Modify: `src/runtime-events.test.ts`

**Interfaces:**
- Consumes: 官方 npm latest 与稳定 manifest 的版本差异。
- Produces: `official_available`（等待验证）、`runtime_available`、`desktop_required`、`skin_unverified`、`up_to_date`、`offline`。

- [ ] **Step 1: 写用户通知去重失败测试**

```rust
#[tokio::test]
async fn official_candidate_notifies_once_without_authorizing_install() {
    let first = coordinator.check(None, official("0.1.2"), stable("0.1.1-rc.2")).await.unwrap();
    let second = coordinator.check(None, official("0.1.2"), stable("0.1.1-rc.2")).await.unwrap();
    assert!(matches!(first.notice, UpdateNotice::OfficialAvailable { .. }));
    assert!(first.compatible_manifest.is_none());
    assert!(!second.should_notify);
}
```

- [ ] **Step 2: 验证旧名称和离线语义失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml coordinator --locked`

Expected: FAIL，旧 variant 为 `OfficialAwaitingCompatibility`，没有显式 `Offline`。

- [ ] **Step 3: 实现稳定状态映射**

前端文案固定为：

```ts
case "official_available": return { heading: "DSH 新版本已发布", body: "正在进行兼容验证，当前版本继续可用。", busy: false };
case "desktop_required": return { heading: "需要先更新桌面客户端", body: "新版 DSH 的核心启动协议需要新版客户端。", busy: false };
case "skin_unverified": return { heading: "DSH 更新可用，皮肤暂未验证", body: "可以更新；更新后会自动恢复官方界面。", busy: false };
case "offline": return { heading: "暂时无法检查更新", body: "已安装的 DSH 仍可离线使用。", busy: false };
```

只有 `runtime_available` 与 `skin_unverified` 提供 runtime 安装动作；`official_available` 和 `desktop_required` 不得保留旧 manifest 安装按钮。

- [ ] **Step 4: 运行全套状态、权限和前端测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml coordinator command_permissions --locked`

Run: `pnpm vitest run src/runtime-events.test.ts src/main.test.ts`

Expected: PASS；通知按 channel+status+version 去重，离线不清除活动 runtime。

- [ ] **Step 5: 提交**

```powershell
git add src-tauri/src/update/coordinator.rs src-tauri/src/update_ui.rs src/runtime-events.ts src/runtime-events.test.ts
git commit -m "feat: surface upstream DSH compatibility status"
```
