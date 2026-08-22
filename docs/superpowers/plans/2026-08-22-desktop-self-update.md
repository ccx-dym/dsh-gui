# Independent Desktop Self-Update Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 DSH Desktop 独立检查、签名验证并安装客户端更新，同时完整保留 DSH runtime 与用户数据。

**Architecture:** 使用 Tauri 2 updater 的 Rust API处理 Windows current-user NSIS 签名下载和安装；本地 Rust 命令封装检查与安装，前端不直接获得插件权限。桌面通道使用与 runtime 不同的 Tauri signing key 和独立 `latest.json`。

**Tech Stack:** Tauri 2.11、`tauri-plugin-updater` 2.10.1、Rust、TypeScript/Vitest、GitHub Actions

**Spec:** `docs/superpowers/specs/2026-08-22-dual-track-updates-first-install-design.md`

## Global Constraints

- runtime key 与 desktop key 必须不同；desktop 私钥只能在 `desktop-release` environment 中使用。
- 客户端更新不得读写 `%LOCALAPPDATA%\DSH Desktop Data\runtimes`、活动指针或用户 generation。
- 只支持 `windows-x86_64` 和版本递增，不开放降级。
- 远程 DSH 页面无客户端 updater 权限；只有 `main`/`updates` 本地窗口可调用封装命令。
- 新增生产依赖固定为 Rust `tauri-plugin-updater = 2.10.1`；不需要前端 updater 包。

---

### Task 1: 隔离桌面更新状态机

**Files:**
- Create: `src-tauri/src/desktop_update.rs`
- Create: `src-tauri/tests/desktop_update.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/domain.rs`

**Interfaces:**
- Consumes: `DesktopUpdateBackend::check()` 与 `DesktopUpdateBackend::install()`。
- Produces: `DesktopUpdateState::{Unavailable, Checking, UpToDate, Available, Downloading, Installing, Failed}` 和 revision envelope。

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn desktop_check_does_not_change_runtime_pointer() {
    let before = fs::read(&active_pointer).unwrap();
    let result = controller.check(&FakeBackend::available("0.1.1")).await.unwrap();
    assert!(matches!(result.state, DesktopUpdateState::Available { version, .. } if version == "0.1.1"));
    assert_eq!(fs::read(&active_pointer).unwrap(), before);
}
```

- [ ] **Step 2: 验证模块缺失**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test desktop_update --locked`

Expected: FAIL，无法导入 `dsh_desktop_lib::desktop_update`。

- [ ] **Step 3: 实现可注入后端与独立 revision**

```rust
pub trait DesktopUpdateBackend: Send + Sync {
    fn check<'a>(&'a self) -> Pin<Box<dyn Future<Output = Result<Option<DesktopRelease>, DesktopUpdateError>> + Send + 'a>>;
    fn install<'a>(&'a self, release: DesktopRelease) -> Pin<Box<dyn Future<Output = Result<(), DesktopUpdateError>> + Send + 'a>>;
}

pub struct DesktopRelease {
    pub version: Version,
    pub notes: Option<String>,
    pub published_at: Option<String>,
}
```

状态文件只允许位于 `settings/desktop-update-state.json`，不得接受外部路径。错误只序列化固定类别 `offline`、`invalid_metadata`、`signature_invalid`、`install_failed`。

- [ ] **Step 4: 运行隔离与状态测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test desktop_update --locked`

Expected: PASS；available、offline、签名失败、并发操作和 runtime pointer 不变均覆盖。

- [ ] **Step 5: 提交**

```powershell
git add src-tauri/src/desktop_update.rs src-tauri/tests/desktop_update.rs src-tauri/src/lib.rs src-tauri/src/domain.rs
git commit -m "feat: model independent desktop updates"
```

---

### Task 2: 接入 Tauri updater 的 Rust-only 边界

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/Cargo.lock`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/desktop_update.rs`
- Modify: `src-tauri/tauri.conf.json`
- Modify: `src-tauri/capabilities/default.json`
- Modify: `src-tauri/tests/command_permissions.rs`

**Interfaces:**
- Consumes: build-time `DSH_DESKTOP_UPDATE_ENDPOINT`、Tauri public key和 signed `latest.json`。
- Produces: local-only commands `get_desktop_update_state`, `check_desktop_update`, `install_desktop_update`。

- [ ] **Step 1: 写权限失败测试**

```rust
#[test]
fn desktop_update_commands_are_local_only() {
    assert!(UPDATE_COMMAND_NAMES.contains(&"get_desktop_update_state"));
    assert!(UPDATE_COMMAND_NAMES.contains(&"check_desktop_update"));
    assert!(UPDATE_COMMAND_NAMES.contains(&"install_desktop_update"));
    assert!(update_command_allowed_for_url("tauri://localhost/index.html"));
    assert!(!update_command_allowed_for_url("http://127.0.0.1:3080/"));
}
```

- [ ] **Step 2: 验证命令和依赖尚不存在**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test command_permissions --locked`

Expected: FAIL，缺少新命令或权限记录。

- [ ] **Step 3: 固定插件并注册 Rust API**

`Cargo.toml` 增加：

```toml
tauri-plugin-updater = "=2.10.1"
```

`lib.rs` setup 注册：

```rust
app.handle().plugin(tauri_plugin_updater::Builder::new().build())?;
```

生产 backend 使用 `tauri_plugin_updater::UpdaterExt`：

```rust
let update = app.updater()?.check().await?;
if let Some(update) = update {
    update.download_and_install(on_chunk, on_finish).await?;
}
```

`tauri.conf.json` 设置 `bundle.createUpdaterArtifacts=true`、`plugins.updater.windows.installMode="passive"`。endpoint 和 public key 由 release workflow 生成临时 config overlay，不提交私钥；本地页面只调用自有命令，因此 capability 不授予插件 updater 权限。

- [ ] **Step 4: 运行权限、编译和 clippy**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test command_permissions --locked`

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets -- -D warnings`

Expected: PASS；远程 origin 调用三个命令均得到 `desktop_update_origin_denied`。

- [ ] **Step 5: 提交**

```powershell
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/lib.rs src-tauri/src/desktop_update.rs src-tauri/tauri.conf.json src-tauri/capabilities/default.json src-tauri/tests/command_permissions.rs
git commit -m "feat: install signed desktop updates"
```

---

### Task 3: 在更新中心分别呈现 desktop 与 DSH 更新

**Files:**
- Create: `src/desktop-update.ts`
- Create: `src/desktop-update.test.ts`
- Modify: `src/main.ts`
- Modify: `src/main.test.ts`
- Modify: `src/styles.css`

**Interfaces:**
- Consumes: `desktop-update-state` event 与三个 local commands。
- Produces: 更新中心两个独立 section：“桌面客户端”和“DSH 运行时”。

- [ ] **Step 1: 写双轨 UI 失败测试**

```ts
it("同时显示客户端更新和 DSH runtime 更新", async () => {
  desktopState = { revision: 2, phase: "available", version: "0.1.1" };
  runtimeState = { ...baseRuntime, phase: "runtime_available", compatibleVersion: "0.1.1-rc.2" };
  renderUpdateCenter(root, desktopState, runtimeState);
  expect(root.textContent).toContain("桌面客户端 0.1.1");
  expect(root.textContent).toContain("DSH 运行时 0.1.1-rc.2");
});
```

- [ ] **Step 2: 验证目前只有 runtime 更新卡片**

Run: `pnpm vitest run src/desktop-update.test.ts src/main.test.ts`

Expected: FAIL，缺少 `renderUpdateCenter`。

- [ ] **Step 3: 实现双轨呈现和互斥操作**

```ts
export interface DesktopUpdateState {
  revision: number;
  phase: "unavailable" | "checking" | "up_to_date" | "available" | "downloading" | "installing" | "failed";
  version?: string;
  notes?: string;
  progressPercent?: number;
}
```

客户端安装按钮文案为“更新 DSH Desktop”，确认框明确“会关闭并重新启动桌面窗口，不会删除 DSH runtime 和数据”。runtime 与 desktop 两个 operation 各自禁用重复点击，不互相改写 revision。

- [ ] **Step 4: 运行前端测试与构建**

Run: `pnpm vitest run src/desktop-update.test.ts src/main.test.ts`

Run: `pnpm build`

Expected: PASS；notes 以 `textContent` 呈现，不能注入 HTML。

- [ ] **Step 5: 提交**

```powershell
git add src/desktop-update.ts src/desktop-update.test.ts src/main.ts src/main.test.ts src/styles.css
git commit -m "feat: show independent desktop updates"
```

---

### Task 4: 发布独立客户端更新

**Files:**
- Create: `.github/workflows/release-desktop.yml`
- Modify: `scripts/tests/workflow-policy.Tests.mjs`
- Modify: `docs/development.md`

**Interfaces:**
- Consumes: tag `desktop-v<version>`、environment secrets `TAURI_SIGNING_PRIVATE_KEY`、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`，variables `TAURI_UPDATER_PUBLIC_KEY` 与 endpoint。
- Produces: current-user NSIS、`.sig` 和静态 `latest.json`。

- [ ] **Step 1: 扩展工作流失败测试**

```js
test("desktop release 使用独立审批环境且不写 runtime stable channel", async () => {
  const workflow = await loadWorkflow(".github/workflows/release-desktop.yml");
  assert.equal(workflow.jobs.release.environment, "desktop-release");
  assert.equal(workflow.jobs.release.permissions.contents, "write");
  assert.doesNotMatch(JSON.stringify(workflow), /releases\/runtime\/stable/);
});
```

- [ ] **Step 2: 验证 workflow 缺失**

Run: `node --test scripts/tests/workflow-policy.Tests.mjs`

Expected: FAIL，指出 `release-desktop.yml` 不存在。

- [ ] **Step 3: 实现 release workflow**

```yaml
name: Release DSH Desktop
on:
  push:
    tags: ['desktop-v*']
permissions:
  contents: read
jobs:
  release:
    runs-on: windows-2025
    timeout-minutes: 45
    environment: desktop-release
    permissions:
      contents: write
```

job 运行 `pnpm install --frozen-lockfile`、`pnpm check`，注入 runtime channel 四项公开配置和 Tauri updater 公钥/私钥，构建 `createUpdaterArtifacts=true` 的 current-user NSIS。`latest.json` 的 `windows-x86_64.signature` 必须是 `.sig` 文件内容，URL 指向 tag 下不可变 EXE。

- [ ] **Step 4: 发布测试版本并执行覆盖安装验收**

使用 `desktop-v0.1.1` 测试：旧版发现更新 → 下载 → Tauri 校验签名 → passive current-user 安装 → 新版启动。安装前后比较 runtime `active.json`、用户 generation 和皮肤设置摘要，三者必须不变。

再用错误签名的本地测试 endpoint 验证安装被拒绝且旧客户端仍能启动；卸载客户端时选择默认卸载路径，确认独立数据目录仍存在，重装后能重新发现 runtime 并复用原用户数据。

- [ ] **Step 5: 提交**

```powershell
git add .github/workflows/release-desktop.yml scripts/tests/workflow-policy.Tests.mjs docs/development.md
git commit -m "ci: release signed desktop updates"
```
