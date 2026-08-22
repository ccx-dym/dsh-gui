# First Install and Runtime Channel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让精简安装版首次启动即可发现、下载、验证并激活 DSH `0.1.1-rc.2`，以后独立提示兼容 runtime 更新。

**Architecture:** 复用现有签名清单、下载、解压、隔离 probe、冷激活和回滚模块，只扩展兼容结论与首次安装呈现。正式客户端在构建时注入公开 npm/manifest endpoint 和 runtime 公钥；远程 DSH 页面继续没有更新命令权限。

**Tech Stack:** Rust/Tauri 2、reqwest、Ed25519、TypeScript/Vitest、现有 update coordinator 与 UpdateUiController

**Spec:** `docs/superpowers/specs/2026-08-22-dual-track-updates-first-install-design.md`

## Global Constraints

- Windows 10/11 x64、current-user NSIS、WebView2。
- 首次安装不打包 runtime，不运行用户系统 npm，不要求源码。
- 核心不兼容阻止 runtime；仅皮肤不兼容允许 runtime 并关闭皮肤。
- 断网或失败保留既有 runtime、活动指针和用户数据。
- 远程 DSH 页面不得获得检查、下载、安装或激活命令。

---

### Task 1: 建模五种发布结论与皮肤兼容标记

**Files:**
- Modify: `src-tauri/src/domain.rs`
- Modify: `src-tauri/src/update/manifest.rs`
- Modify: `runtime/manifest.schema.json`
- Modify: `src-tauri/tests/manifest.rs`
- Modify: `src-tauri/src/update/coordinator.rs`

**Interfaces:**
- Consumes: signed manifest fields `core_compatibility` and `skin_compatibility`.
- Produces: `UpdateNotice::{OfficialAvailable, RuntimeAvailable, DesktopRequired, SkinUnverified, UpToDate, Offline}`.

- [ ] **Step 1: 写失败测试覆盖决策矩阵**

```rust
#[test]
fn compatible_core_with_unverified_skin_allows_runtime() {
    let notice = decide_notice(Some(v("0.1.1-rc.1")), official("0.1.1-rc.2"), manifest("0.1.1-rc.2", "compatible", "unverified"));
    assert!(matches!(notice, UpdateNotice::SkinUnverified { compatible, .. } if compatible == "0.1.1-rc.2"));
}

#[test]
fn newer_minimum_desktop_blocks_runtime() {
    let notice = decide_notice(Some(v("0.1.1-rc.1")), official("0.1.1-rc.2"), manifest_requiring_desktop("0.2.0"));
    assert!(matches!(notice, UpdateNotice::DesktopRequired { minimum_desktop, .. } if minimum_desktop == "0.2.0"));
}
```

- [ ] **Step 2: 验证旧模型无法表达结果**

Run: `cargo test --manifest-path src-tauri/Cargo.toml coordinator --locked`

Expected: FAIL，缺少 `SkinUnverified` 和 `DesktopRequired` variants。

- [ ] **Step 3: 最小实现新 manifest 字段和决策**

manifest schema 增加必填枚举：

```json
"core_compatibility": { "enum": ["compatible", "desktop_required"] },
"skin_compatibility": { "enum": ["verified", "unverified"] }
```

Rust 中使用：

```rust
pub enum CompatibilityLevel { Compatible, DesktopRequired }
pub enum SkinCompatibility { Verified, Unverified }
```

决策优先级固定为：网络失败且有旧 runtime → `Offline`；manifest 要求更高桌面版本或核心标记不兼容 → `DesktopRequired`；核心兼容且皮肤未验证 → `SkinUnverified`；核心和皮肤均验证 → `RuntimeAvailable`。

- [ ] **Step 4: 运行 Rust 更新测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml manifest coordinator --locked`

Expected: PASS，六种状态均序列化为 snake_case。

- [ ] **Step 5: 提交**

```powershell
git add src-tauri/src/domain.rs src-tauri/src/update/manifest.rs runtime/manifest.schema.json src-tauri/tests/manifest.rs src-tauri/src/update/coordinator.rs
git commit -m "feat: classify runtime and skin compatibility"
```

---

### Task 2: 首次安装状态与真实下载进度

**Files:**
- Modify: `src-tauri/src/update_ui.rs`
- Modify: `src-tauri/src/update/download.rs`
- Modify: `src/runtime-events.ts`
- Modify: `src/runtime-events.test.ts`
- Modify: `src/main.ts`
- Modify: `src/main.test.ts`

**Interfaces:**
- Consumes: `DownloadProgress { downloaded_bytes: u64, total_bytes: Option<u64> }`.
- Produces: `UpdateUiState.downloaded_bytes`, `download_percent` 与首次安装按钮 `安装 DSH <version>`。

- [ ] **Step 1: 写首次安装 UI 和进度失败测试**

```ts
it("首次安装直接展示已验证 rc.2 的安装动作", () => {
  const view = updatePresentation({ ...base, phase: "runtime_available", compatibleVersion: "0.1.1-rc.2", artifactSize: 108024750 });
  expect(view.heading).toBe("安装 DSH 0.1.1-rc.2");
  expect(view.primaryAction?.command).toBe("install_compatible_update");
});

it("下载状态显示实际百分比", () => {
  const view = updatePresentation({ ...base, phase: "downloading", downloadedBytes: 50, artifactSize: 100 });
  expect(view.details).toBe("50% · 50 B / 100 B");
});
```

- [ ] **Step 2: 验证现有 UI 不满足文案和进度**

Run: `pnpm vitest run src/runtime-events.test.ts src/main.test.ts`

Expected: FAIL，现有 phase 为 `compatible_available` 且没有 `downloadedBytes`。

- [ ] **Step 3: 扩展状态并接通受限进度回调**

Rust 状态增加：

```rust
pub downloaded_bytes: Option<u64>,
pub skin_compatible: Option<bool>,
```

下载器公开：

```rust
pub trait DownloadProgressSink: Send + Sync {
    fn report(&self, downloaded_bytes: u64, total_bytes: Option<u64>);
}
```

每个 chunk 只更新内存状态并节流到最多每 100ms 一个 Tauri event；不把 URL、header 或响应正文发给前端。首次无 runtime 时，签名清单加载成功后直接呈现版本、大小、验证摘要和“安装 DSH 0.1.1-rc.2”。

- [ ] **Step 4: 运行 Rust 与前端状态机测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml update_ui download --locked`

Run: `pnpm vitest run src/runtime-events.test.ts src/main.test.ts`

Expected: PASS；0 字节、未知总量、超量 chunk 均不会产生大于 100 的百分比。

- [ ] **Step 5: 提交**

```powershell
git add src-tauri/src/update_ui.rs src-tauri/src/update/download.rs src/runtime-events.ts src/runtime-events.test.ts src/main.ts src/main.test.ts
git commit -m "feat: guide first-time DSH runtime installation"
```

---

### Task 3: 皮肤不兼容时自动恢复官方界面

**Files:**
- Modify: `src-tauri/src/skin/adapter.rs`
- Modify: `src-tauri/src/skin/controller.rs`
- Modify: `src-tauri/src/update_ui.rs`
- Modify: `src-tauri/tests/skin_adapter.rs`
- Modify: `src/runtime-events.ts`

**Interfaces:**
- Consumes: 活动 runtime exact version 与 `skin_compatibility`。
- Produces: `SkinRuntimePolicy { enabled: bool, reason: Option<SkinDisableReason> }`。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn rc2_without_adapter_keeps_core_and_disables_skin() {
    let policy = skin_runtime_policy(&Version::parse("0.1.1-rc.2").unwrap(), SkinCompatibility::Unverified);
    assert!(!policy.enabled);
    assert_eq!(policy.reason, Some(SkinDisableReason::VersionUnverified));
}
```

- [ ] **Step 2: 验证当前只有 `adapter_for(...).is_none()`**

Run: `cargo test --manifest-path src-tauri/Cargo.toml skin_adapter --locked`

Expected: FAIL，缺少 `skin_runtime_policy`。

- [ ] **Step 3: 实现 fail-closed 皮肤策略**

```rust
pub fn skin_runtime_policy(version: &Version, status: SkinCompatibility) -> SkinRuntimePolicy {
    let enabled = status == SkinCompatibility::Verified && adapter_for(version).is_some();
    SkinRuntimePolicy {
        enabled,
        reason: (!enabled).then_some(SkinDisableReason::VersionUnverified),
    }
}
```

激活 rc.2 后不删除用户选择的图片或设置，只停止注入并移除现有皮肤 DOM/style 节点；状态提示“当前版本皮肤未验证，已恢复官方界面”。不得影响 `AppController` 启动 DSH WebUI。

- [ ] **Step 4: 运行皮肤、生命周期和前端测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml skin_adapter lifecycle --locked`

Run: `pnpm test`

Expected: PASS，rc.2 WebUI 仍启动，远程页面仍只能调用 `report_skin_adapter`。

- [ ] **Step 5: 提交**

```powershell
git add src-tauri/src/skin/adapter.rs src-tauri/src/skin/controller.rs src-tauri/src/update_ui.rs src-tauri/tests/skin_adapter.rs src/runtime-events.ts
git commit -m "feat: disable unverified skins without blocking DSH"
```

---

### Task 4: 正式构建注入 runtime 通道并做首次安装实测

**Files:**
- Modify: `src-tauri/build.rs`
- Modify: `scripts/smoke-desktop.ps1`
- Modify: `docs/development.md`

**Interfaces:**
- Consumes: 四个公开构建变量和已发布 stable manifest。
- Produces: 可用的精简 NSIS，而不是“发布通道尚未配置”的测试构建。

- [ ] **Step 1: 增加 build 配置失败门禁**

```rust
for key in [
    "DSH_DESKTOP_NPM_REGISTRY_ROOT",
    "DSH_DESKTOP_COMPAT_MANIFEST_URL",
    "DSH_DESKTOP_COMPAT_SIGNATURE_URL",
    "DSH_DESKTOP_COMPAT_PUBLIC_KEY",
] {
    println!("cargo:rerun-if-env-changed={key}");
}
```

`smoke-desktop.ps1 -RequireReleaseChannel` 必须在任一配置缺失时退出 1。

- [ ] **Step 2: 验证未注入配置时门禁失败**

Run: `pwsh -NoProfile -File scripts/smoke-desktop.ps1 -RequireReleaseChannel`

Expected: FAIL，固定错误码 `release_channel_missing`。

- [ ] **Step 3: 用公开固定地址构建**

```powershell
$env:DSH_DESKTOP_NPM_REGISTRY_ROOT = 'https://registry.npmjs.org/'
$env:DSH_DESKTOP_COMPAT_MANIFEST_URL = 'https://raw.githubusercontent.com/ccx-dym/dsh-gui/main/releases/runtime/stable/manifest.json'
$env:DSH_DESKTOP_COMPAT_SIGNATURE_URL = 'https://raw.githubusercontent.com/ccx-dym/dsh-gui/main/releases/runtime/stable/manifest.sig'
$env:DSH_DESKTOP_COMPAT_PUBLIC_KEY = $env:DSH_RUNTIME_PUBLIC_KEY_HEX
$env:DSH_DESKTOP_UPDATE_CHANNEL = 'stable'
pnpm tauri build
```

- [ ] **Step 4: 在干净测试数据目录执行 Windows 验收**

安装新的 NSIS，确认：首次启动显示 rc.2 → 确认下载 → 摘要/签名/安全解压/probe → 冷启动激活 → DSH WebUI 可配置模型和工作区；随后断网重启仍可用。错误签名 fixture 必须拒绝并保持未安装，下载中断不得生成 active pointer。

- [ ] **Step 5: 提交门禁与实测记录**

```powershell
git add src-tauri/build.rs scripts/smoke-desktop.ps1 docs/development.md
git commit -m "test: verify first-install runtime channel"
```
