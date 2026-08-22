# DSH Desktop Phase 3 Immersive Skins Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 DSH Desktop 增加安全的本地图片导入、独立设置窗口、实时预览和对官方 DSH WebUI 失败关闭的沉浸式皮肤适配。

**Architecture:** Rust `skin` 模块拥有图片验证、不可变导入、设置持久化、只读协议和版本适配状态；只有本地 `appearance` 窗口能调用设置命令。主 WebView 导航到已验证的官方回环页面后，由桌面壳注入独立 GPU 背景层和 CSS 变量；DOM 或版本检查失败时撤销全部样式，官方页面业务逻辑保持原样。

**Tech Stack:** Tauri 2.11.5、Rust 2024、WebView2、TypeScript 7、Vitest、`tauri-plugin-dialog = 2.7.2`、`image = 0.25.10`（仅 PNG/JPEG/WebP decoder）。

**Spec:** `docs/superpowers/specs/2026-08-21-dsh-desktop-design.md` 第 6、9、10、11、12 节，以及 `docs/superpowers/plans/2026-08-21-dsh-desktop-roadmap.md` 阶段 3。

## Global Constraints

- 仅支持 Windows 10/11 x64，保持 Tauri 2 与单主 WebView2。
- 不 fork、不修改、不代理官方 DSH；皮肤失败只能回退官方视觉，不能阻断 Agent、会话、模型、工具或审批。
- 当前程序目录为 `%LOCALAPPDATA%\DSH Desktop`；可保留皮肤数据固定在 `%LOCALAPPDATA%\DSH Desktop Data\skins`，不得重新合并。
- 只接受 PNG、JPG/JPEG、WebP；文件上限 `20 * 1024 * 1024` 字节；最大边 `7680`，总像素不超过 `7680 * 4320`。
- 背景只解码一次并使用固定合成层；模糊只作用于背景层，不作用于正文和控件。
- 恢复默认、更换皮肤和取消设置不得删除已导入图片；任何清理功能不属于本阶段。
- 新增生产依赖仅为已批准的 `tauri-plugin-dialog = 2.7.2` 与 `image = 0.25.10`，并更新 `Cargo.lock`；不增加前端生产依赖。
- 所有命令和公共函数使用类型化参数/返回值、中文 Sphinx 风格 Docstring、稳定错误码和脱敏诊断；不得记录源图片路径或文件名。
- 每个任务先写失败测试，再做最小实现，通过独立审查后提交。

---

## File Structure

- `src-tauri/src/skin/model.rs`：皮肤设置、图片元数据、填充/位置/遮罩枚举和数值边界。
- `src-tauri/src/skin/store.rs`：严格 JSON、原子保存、revision 并发控制与默认恢复。
- `src-tauri/src/skin/import.rs`：受限读取、真实格式/尺寸/完整解码验证、SHA-256 不可变复制。
- `src-tauri/src/skin/protocol.rs`：只读 `dsh-skin://localhost/<digest>` 请求解析和响应。
- `src-tauri/src/skin/adapter.rs`：DSH 版本 allowlist、注入脚本生成、DOM 检查和失败撤销。
- `src-tauri/src/skin/controller.rs`：Tauri 命令、原生文件选择器、状态发布和主窗口应用。
- `src-tauri/src/skin/mod.rs`：模块公开接口与命令名称常量。
- `src-tauri/tests/skin_store.rs`：持久化、并发 revision、损坏设置和不删除历史的集成测试。
- `src-tauri/tests/skin_import.rs`：格式、大小、尺寸、损坏图片、Unicode 路径和不可变导入测试。
- `src-tauri/tests/skin_protocol.rs`：协议越权、摘要绑定、MIME、缓存头和 reparse/hardlink 测试。
- `src-tauri/tests/skin_adapter.rs`：版本/DOM 合约、脚本转义、失败撤销和业务边界测试。
- `src/skin-state.ts`：前端草稿 reducer、数值收敛和展示状态。
- `src/skin-state.test.ts`：前端状态测试。
- `src/appearance.ts`：设置窗口 UI、实时预览和保存/恢复默认交互。
- `src/appearance.test.ts`：DOM、键盘、错误和防重复提交测试。
- `src/appearance.css`：天空蓝设置界面与独立预览层。
- `src/main.ts`：按 `?view=appearance` 分派本地入口，不让设置 UI 混入主启动页。
- `src-tauri/capabilities/appearance.json`：只给本地 `appearance` 窗口皮肤命令和 dialog 权限。
- `src-tauri/capabilities/skin-report.json`：只给 `main` 的数字回环来源单一、无数据读取能力的适配报告命令。
- `src-tauri/tauri.conf.json`：隐藏的 `appearance` 本地窗口。
- `src-tauri/src/tray.rs`：稳定 `appearance` 菜单项及窗口恢复。
- `docs/development.md`：手工矩阵与性能口径。

---

### Task 1: Typed skin model and atomic settings store

**Files:**
- Create: `src-tauri/src/skin/mod.rs`
- Create: `src-tauri/src/skin/model.rs`
- Create: `src-tauri/src/skin/store.rs`
- Create: `src-tauri/tests/skin_store.rs`
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**
- Produces: `SkinSettings`, `SkinDraft`, `SkinImage`, `SkinStateEnvelope`, `SkinStore::load()`, `SkinStore::save(expected_revision, draft)`, `SkinStore::reset(expected_revision)`.
- Persists: `%APPDATA%\DSH Desktop\settings\skin.json`, schema `1`, deny unknown fields.

- [ ] **Step 1: Write failing model and store tests**

```rust
#[test]
fn defaults_are_non_immersive_and_bounded() {
    let settings = SkinSettings::default();
    assert!(!settings.immersive);
    assert_eq!(settings.fit, SkinFit::Cover);
    assert_eq!(settings.position, SkinPosition::Center);
    assert_eq!(settings.blur_px, 0);
    assert_eq!(settings.mask_opacity_percent, 22);
    assert_eq!(settings.panel_opacity_percent, 88);
}

#[test]
fn stale_revision_cannot_overwrite_newer_skin_settings() {
    let store = fixture_store("stale-revision");
    let first = store.save(0, valid_draft()).expect("first save");
    let error = store.save(0, valid_draft()).expect_err("stale save");
    assert_eq!(error.kind(), SkinErrorKind::RevisionConflict);
    assert_eq!(store.load().expect("load").revision, first.revision);
}

#[test]
fn reset_changes_only_the_pointer_and_keeps_imported_files() {
    let store = fixture_store("reset-keeps-history");
    let image = seed_imported_image(&store);
    store.save(0, draft_for(&image)).expect("save");
    let reset = store.reset(1).expect("reset");
    assert!(!reset.settings.immersive);
    assert!(image.path.exists(), "恢复默认不得删除历史图片");
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test skin_store --locked -- --nocapture`

Expected: compilation fails because `dsh_desktop_lib::skin` does not exist.

- [ ] **Step 3: Implement strict types and validation**

```rust
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkinFit { #[default] Cover, Contain, Stretch, Center }

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkinPosition {
    TopLeft, Top, TopRight, Left, #[default] Center, Right, BottomLeft, Bottom, BottomRight,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaskTone { #[default] Light, Dark }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkinDraft {
    pub immersive: bool,
    pub image_digest: Option<String>,
    pub fit: SkinFit,
    pub position: SkinPosition,
    pub blur_px: u8,
    pub mask_tone: MaskTone,
    pub mask_opacity_percent: u8,
    pub panel_opacity_percent: u8,
}
```

Validation must reject blur above `32`, mask opacity above `80`, panel opacity outside `55..=100`, non-canonical lowercase 64-hex digests, and `immersive=true` without a registered image. Save with the existing atomic-file pattern and revalidate target/parent identities before replace.

- [ ] **Step 4: Run store tests GREEN and all Rust tests**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test skin_store --locked -- --nocapture`

Expected: all `skin_store` tests pass.

Run: `cargo test --manifest-path src-tauri/Cargo.toml --locked`

Expected: existing runtime/update tests and new store tests pass.

- [ ] **Step 5: Commit**

```powershell
git add src-tauri/src/skin src-tauri/tests/skin_store.rs src-tauri/src/lib.rs
git commit -m "feat: persist bounded skin settings"
```

---

### Task 2: Validate and immutably import selected images

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/Cargo.lock`
- Create: `src-tauri/src/skin/import.rs`
- Create: `src-tauri/tests/skin_import.rs`

**Interfaces:**
- Consumes: `SkinImage`, `SkinError`, fixed `AppPaths.skins`.
- Produces: `SkinImporter::import(&Path) -> Result<SkinImage, SkinError>`; destination `<sha256>.<png|jpg|webp>`.

- [ ] **Step 1: Add failing fixture tests without adding dependencies yet**

```rust
#[test]
fn imports_valid_png_by_content_not_extension() {
    let source = fixture_png_named("天空背景.txt", 1920, 1080);
    let image = importer().import(&source).expect("valid PNG");
    assert_eq!(image.format, SkinFormat::Png);
    assert_eq!(image.width, 1920);
    assert_eq!(image.height, 1080);
    assert!(image.path.ends_with(format!("{}.png", image.digest)));
}

#[test]
fn rejects_oversize_dimension_corruption_and_unsupported_gif() {
    assert_kind(importer().import(&fixture_bytes(20 * 1024 * 1024 + 1)), SkinErrorKind::TooLarge);
    assert_kind(importer().import(&fixture_png_header(7681, 100)), SkinErrorKind::Dimensions);
    assert_kind(importer().import(&fixture_corrupt_png()), SkinErrorKind::Decode);
    assert_kind(importer().import(&fixture_gif()), SkinErrorKind::UnsupportedFormat);
}

#[test]
fn importing_same_bytes_is_idempotent_and_never_overwrites_different_content() {
    let first = importer().import(&fixture_webp()).expect("first");
    let second = importer().import(&fixture_webp()).expect("second");
    assert_eq!(first, second);
    assert_eq!(sha256(&first.path), first.digest);
}
```

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test skin_import --locked -- --nocapture`

Expected: compilation fails because `SkinImporter` is missing.

- [ ] **Step 3: Add the approved minimal dependencies**

```toml
[dependencies]
image = { version = "=0.25.10", default-features = false, features = ["jpeg", "png", "webp"] }
tauri-plugin-dialog = "=2.7.2"
```

Run: `cargo update --manifest-path src-tauri/Cargo.toml -p image --precise 0.25.10`

Expected: `Cargo.lock` records only the dependency closure required by the two approved crates.

- [ ] **Step 4: Implement bounded read, decode and immutable copy**

Read at most `20 MiB + 1`, hash exact bytes, detect format from content, set `image::Limits` before full decode, require `max(width,height) <= 7680` and `width * height <= 33_177_600`, and perform decode in `tauri::async_runtime::spawn_blocking`. Create with `create_new(true)`; if the digest target exists, rehash and accept only exact matching bytes. Reject source/destination reparse points and multi-link destination files.

```rust
pub const MAX_SKIN_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_SKIN_EDGE: u32 = 7680;
pub const MAX_SKIN_PIXELS: u64 = 7680 * 4320;

pub async fn import(&self, source: PathBuf) -> Result<SkinImage, SkinError> {
    let root = self.root.clone();
    tauri::async_runtime::spawn_blocking(move || import_blocking(&root, &source))
        .await
        .map_err(|_| SkinError::worker())?
}
```

- [ ] **Step 5: Run tests, strict Clippy and commit**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test skin_import --locked -- --nocapture`

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets -- -D warnings`

Expected: both pass; no source path or dynamic decoder error is serialized.

```powershell
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/skin/import.rs src-tauri/tests/skin_import.rs
git commit -m "feat: import validated skin images"
```

---

### Task 3: Read-only skin resource protocol

**Files:**
- Create: `src-tauri/src/skin/protocol.rs`
- Create: `src-tauri/tests/skin_protocol.rs`
- Modify: `src-tauri/src/skin/mod.rs`
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**
- Consumes: current registered `SkinImage` from `SkinStore`.
- Produces: exact resource URL `dsh-skin://localhost/<digest>` and `handle_skin_request(uri) -> http::Response<Vec<u8>>`.

- [ ] **Step 1: Write protocol boundary tests**

```rust
#[test]
fn serves_only_the_registered_digest_with_fixed_headers() {
    let protocol = fixture_protocol("registered");
    let response = protocol.request(&format!("dsh-skin://localhost/{}", protocol.digest()));
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["Content-Type"], "image/png");
    assert_eq!(response.headers()["Cache-Control"], "private, max-age=31536000, immutable");
}

#[test]
fn rejects_query_fragment_encoding_traversal_wrong_host_and_other_digest() {
    for uri in [
        "dsh-skin://localhost/../settings/skin.json",
        "dsh-skin://localhost/%2e%2e/settings",
        "dsh-skin://localhost/abcd?path=C%3A%5Csecret",
        "dsh-skin://evil/abcd",
        "dsh-skin://localhost/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
    ] {
        assert_eq!(fixture_protocol("deny").request(uri).status(), 404);
    }
}
```

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test skin_protocol --locked -- --nocapture`

Expected: compilation fails because `SkinProtocol` is missing.

- [ ] **Step 3: Implement exact URI-to-registered-image mapping**

The handler must parse scheme/host/path as typed URL parts, reject query/userinfo/fragment/port, compare one canonical digest segment to the store snapshot, open the already registered canonical file with identity guards, bound read by `MAX_SKIN_BYTES`, rehash before response, and return only fixed MIME/cache/ETag headers. It must never convert request text into a filesystem path.

Register `dsh-skin` before `.setup(...)` and keep protocol errors as fixed `404`/`500` bodies with no paths.

- [ ] **Step 4: Run protocol, command-permission and full Rust tests**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test skin_protocol --locked -- --nocapture`

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test command_permissions --locked`

Expected: protocol tests pass and existing remote update-command denial remains unchanged.

- [ ] **Step 5: Commit**

```powershell
git add src-tauri/src/skin src-tauri/tests/skin_protocol.rs src-tauri/src/lib.rs
git commit -m "feat: serve the active skin through a read-only protocol"
```

---

### Task 4: Local appearance window, native picker and command ACL

**Files:**
- Create: `src-tauri/src/skin/controller.rs`
- Create: `src-tauri/capabilities/appearance.json`
- Modify: `src-tauri/src/skin/mod.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/tauri.conf.json`
- Modify: `src-tauri/tests/command_permissions.rs`

**Interfaces:**
- Produces commands: `get_skin_state`, `choose_skin_image`, `save_skin_settings`, `reset_skin_settings`.
- Produces event: `skin-state` with monotonic `revision`.
- Window: label `appearance`, URL `index.html?view=appearance`, hidden by default, `760×720`, minimum `680×620`.

- [ ] **Step 1: Write failing capability and window tests**

```rust
#[test]
fn appearance_window_is_local_hidden_and_owns_every_skin_mutation_command() {
    let config = tauri_config();
    assert_window(&config, "appearance", "index.html?view=appearance", false);
    let appearance = capability("appearance.json");
    assert_eq!(appearance.windows, ["appearance"]);
    assert!(appearance.permissions.contains("allow-choose-skin-image"));
    assert!(!capability("default.json").permissions.iter().any(|p| p.contains("skin")));
}
```

- [ ] **Step 2: Run permissions tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test command_permissions --locked appearance_window_is_local_hidden_and_owns_every_skin_mutation_command -- --exact`

Expected: FAIL because the window and capability do not exist.

- [ ] **Step 3: Implement commands and native picker**

```rust
#[tauri::command]
pub async fn choose_skin_image(
    app: AppHandle,
    controller: State<'_, SkinController>,
) -> Result<Option<SkinImageView>, SkinCommandError> {
    let selected = app.dialog().file()
        .add_filter("图片", &["png", "jpg", "jpeg", "webp"])
        .blocking_pick_file();
    let Some(path) = selected.and_then(|value| value.into_path().ok()) else { return Ok(None); };
    controller.import(path).await.map(Some).map_err(Into::into)
}
```

The serialized view returns digest, format, width, height, byte size and protocol URL only; it never returns the source or managed filesystem path. `save` and `reset` require `expectedRevision`; concurrent/stale operations return `revision_conflict`.

- [ ] **Step 4: Register plugin, state, commands and strict capability**

Register `.plugin(tauri_plugin_dialog::init())`. Give `appearance.json` only core event listen/unlisten, four generated skin permissions, and the dialog open permission required by the Rust picker. Do not add skin commands to `local-main`.

- [ ] **Step 5: Run focused tests, full Rust tests and commit**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test command_permissions --locked`

Run: `cargo test --manifest-path src-tauri/Cargo.toml --locked`

```powershell
git add src-tauri/src/skin src-tauri/src/lib.rs src-tauri/tauri.conf.json src-tauri/capabilities src-tauri/tests/command_permissions.rs
git commit -m "feat: expose local-only skin settings commands"
```

---

### Task 5: Appearance UI and live preview

**Files:**
- Create: `src/skin-state.ts`
- Create: `src/skin-state.test.ts`
- Create: `src/appearance.ts`
- Create: `src/appearance.test.ts`
- Create: `src/appearance.css`
- Modify: `src/main.ts`

**Interfaces:**
- Consumes: the four commands and `skin-state` event from Task 4.
- Produces: an accessible draft editor; preview changes remain local until `save_skin_settings`.

- [ ] **Step 1: Write reducer and DOM tests**

```ts
it("clamps every visual control before it can reach Rust", () => {
  const state = reduceSkinDraft(initialSkinState(), {
    type: "visuals",
    blurPx: 99,
    maskOpacityPercent: -4,
    panelOpacityPercent: 12,
  });
  expect(state.draft.blurPx).toBe(32);
  expect(state.draft.maskOpacityPercent).toBe(0);
  expect(state.draft.panelOpacityPercent).toBe(55);
});

it("renders one composited background and never places blur on content", () => {
  renderAppearance(root, stateWithImage());
  expect(root.querySelectorAll("[data-skin-background]")).toHaveLength(1);
  expect(root.querySelector("[data-skin-background]")).toHaveStyle({ filter: "blur(12px)" });
  expect(root.querySelector("[data-skin-preview-content]")).not.toHaveStyle({ filter: expect.anything() });
});
```

- [ ] **Step 2: Run frontend tests and verify RED**

Run: `pnpm test -- src/skin-state.test.ts src/appearance.test.ts`

Expected: FAIL because the appearance modules do not exist.

- [ ] **Step 3: Implement the typed draft reducer and view dispatcher**

```ts
const view = new URLSearchParams(window.location.search).get("view");
if (view === "appearance") {
  void initializeAppearance(root);
} else {
  void initializeDesktop(root);
}
```

Controls: image chooser, immersive switch, fit select (`cover/contain/stretch/center`), nine-position select, blur `0..32`, tone `light/dark`, mask `0..80`, panel `55..100`, Save, Restore default. Image selection imports a managed copy and updates only the draft. Save uses current revision; reset requires the fixed Chinese confirmation text and does not claim files were deleted.

- [ ] **Step 4: Implement a single-layer preview and accessibility**

Use an absolutely positioned `data-skin-background` with `background-image: url(...)`, `will-change: transform`, `transform: scale(...)` to hide blur edges, `pointer-events:none`, and no animation. The preview content uses a separate surface with `background: rgb(... / var(--panel-opacity))`; labels have explicit accessible names and keyboard focus styles.

- [ ] **Step 5: Run frontend tests/build and commit**

Run: `pnpm test`

Run: `pnpm build`

Expected: all tests pass; no image bytes/base64 appear in DOM or JS state.

```powershell
git add src/skin-state.ts src/skin-state.test.ts src/appearance.ts src/appearance.test.ts src/appearance.css src/main.ts
git commit -m "feat: add immersive skin preview settings"
```

---

### Task 6: Version-bound DSH adapter and fail-closed injection

**Files:**
- Create: `src-tauri/src/skin/adapter.rs`
- Create: `src-tauri/tests/skin_adapter.rs`
- Create: `src-tauri/capabilities/skin-report.json`
- Modify: `src-tauri/src/skin/controller.rs`
- Modify: `src-tauri/src/app_controller.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/tests/command_permissions.rs`

**Interfaces:**
- Consumes: active deployment DSH version, `SkinSettings`, protocol URL and main numeric-loopback URL.
- Produces: adapter `dsh-0.1.1-rc.1-v1`, `report_skin_adapter(adapter_version, compatible)`, and `apply_to_main`.

- [ ] **Step 1: Write adapter contract tests**

```rust
#[test]
fn supports_only_the_exact_verified_dsh_version() {
    assert!(adapter_for(&Version::parse("0.1.1-rc.1").unwrap()).is_some());
    assert!(adapter_for(&Version::parse("0.1.2").unwrap()).is_none());
}

#[test]
fn script_checks_dom_before_inserting_one_pointer_transparent_layer() {
    let script = adapter_script(&fixture_settings()).expect("script");
    assert!(script.contains("document.querySelector('#root')"));
    assert!(script.contains("--dsw-alias-bg-base"));
    assert!(script.contains("pointer-events:none"));
    assert!(script.contains("dsh-desktop-skin-background"));
    assert!(!script.contains("fetch("));
    assert!(!script.contains("XMLHttpRequest"));
    assert!(!script.contains("addEventListener('click'"));
}

#[test]
fn unsupported_or_disabled_state_generates_only_cleanup_script() {
    let script = cleanup_script();
    assert!(script.contains("dsh-desktop-skin-style"));
    assert!(script.contains("dsh-desktop-skin-background"));
    assert!(!script.contains("dsh-skin://"));
}
```

- [ ] **Step 2: Run adapter tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test skin_adapter --locked -- --nocapture`

Expected: compilation fails because `adapter_for` and script builders are missing.

- [ ] **Step 3: Implement the exact v1 DOM contract**

Before applying, require all of: exact DSH version `0.1.1-rc.1`, numeric `127.0.0.1` origin, `#root`, and non-empty computed variables `--dsw-alias-bg-base`, `--dsw-alias-bg-layer-1`, `--dsw-alias-bg-layer-2`. The injected IIFE first removes prior desktop nodes, then inserts one fixed background and one style element. It changes only root/body background, the three surface variables, border opacity and the desktop-owned nodes; no event interception, timers, fetch, storage or DSH DOM mutations.

If any check fails, remove both desktop nodes, restore properties by removing the style element, and report `compatible=false`. `compatible=true` is accepted only when the native active version already maps to the same adapter; a false report may disable skin but cannot grant capabilities.

- [ ] **Step 4: Add the one-command remote capability**

`skin-report.json` targets window `main`, remote URL `http://127.0.0.1:*`, and permission `allow-report-skin-adapter` only. Tests must assert it does not include runtime, update, dialog, filesystem, shell or event permissions. All mutating/read commands remain unavailable to the official page.

- [ ] **Step 5: Wire injection to every successful official navigation**

After the existing ready event navigates to the exact numeric-loopback URL, queue `apply_to_main`; on navigation back to the local page, stop, failure, unsupported version or disabled setting, queue cleanup. A skin eval error records only `DiagnosticStage::SkinApply` and `DiagnosticErrorKind::TauriError`; it never changes runtime status.

- [ ] **Step 6: Run all security tests and commit**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test skin_adapter --locked`

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test command_permissions --locked`

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets -- -D warnings`

```powershell
git add src-tauri/src/skin src-tauri/src/app_controller.rs src-tauri/src/lib.rs src-tauri/capabilities src-tauri/tests
git commit -m "feat: apply versioned fail-closed DSH skins"
```

---

### Task 7: Tray entry and window lifecycle

**Files:**
- Modify: `src-tauri/src/tray.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/tests/command_permissions.rs`

**Interfaces:**
- Adds stable tray ID `appearance` with label `选择或设置皮肤`.
- Close on `appearance` hides only that window; main close-to-tray and explicit exit semantics remain unchanged.

- [ ] **Step 1: Write failing tray tests**

```rust
#[test]
fn appearance_action_opens_only_the_local_settings_window() {
    let ui = TestDesktopUi::default();
    handle_tray_action(TrayAction::Appearance, &controller(), &ui).expect("open");
    assert_eq!(ui.calls(), ["show_appearance", "focus_appearance"]);
}

#[test]
fn closing_appearance_hides_it_without_changing_runtime_or_exit_state() {
    assert_eq!(close_decision_for("appearance", false), CloseDecision::HideWindow);
}
```

- [ ] **Step 2: Run tray tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib tray::tests --locked`

Expected: compilation fails because `TrayAction::Appearance` is missing.

- [ ] **Step 3: Implement the minimal menu and close route**

Insert `appearance` between Hide and Restart. Extend `DesktopUi` with `show_appearance`/`focus_appearance`; do not route it through runtime restart or update state. Extend the window close handler from `main|updates` to `main|updates|appearance`, hiding `appearance` like `updates`.

- [ ] **Step 4: Run tray/full tests and commit**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --locked`

```powershell
git add src-tauri/src/tray.rs src-tauri/src/lib.rs src-tauri/tests/command_permissions.rs
git commit -m "feat: open skin settings from the tray"
```

---

### Task 8: Real WebView2 acceptance and performance regression gate

**Files:**
- Modify: `docs/development.md`
- Modify: `scripts/smoke-runtime.ps1`
- Test: installed `src-tauri/target/release/bundle/nsis/DSH Desktop_0.1.0_x64-setup.exe`

**Interfaces:**
- Consumes: Phase 2 signed runtime fixture and an actual user-selected test PNG/JPEG/WebP.
- Produces: reproducible default-vs-skin performance record and Windows GUI matrix.

- [ ] **Step 1: Extend the runtime smoke assertions without touching real user data**

Add focused Rust invocations for invalid image format, over-limit dimensions, protocol traversal, unsupported DSH adapter fallback and command ACL. Keep `-SecurityFixturesOnly` independent of large images and do not delete its audit directory.

- [ ] **Step 2: Run the complete automated gate**

Run: `pnpm check`

Expected: Rust fmt/tests/strict Clippy,  frontend tests/build and security fixtures all pass.

- [ ] **Step 3: Build and install a current-user RC**

Run: `pnpm tauri build --bundles nsis`

Record installer byte size, SHA-256 and Authenticode status. Install over the existing personal RC; do not run the uninstaller and do not delete old managed skins.

- [ ] **Step 4: Perform GUI acceptance with Computer Use**

Verify: tray opens `appearance`; picker filters four extensions; valid PNG/JPEG/WebP previews; source rename/removal after import does not break the managed copy; cover/contain/stretch/center and nine positions work; blur affects only background; tone/mask/panel controls remain readable; Save survives restart; Restore default keeps imported files; invalid/corrupt/oversize/8K+ files show stable Chinese errors; official chat, streaming, tool approval, file picker and terminal remain interactive; unsupported DOM/version visibly reports `未验证` and fully restores the official UI.

- [ ] **Step 5: Measure before/after performance**

On the same Windows/WebView2 build, record default and 8K-skin values for: process-to-clickable window, DSH double-gate ready, 60-second foreground CPU, 60-second tray CPU, desktop Working Set/Private Bytes, WebView2 process-group memory, and scroll responsiveness. Pass if startup stays under 5 seconds, DSH ready under 10 seconds, idle CPU remains near 0%, and scrolling shows no repeated image decode or obvious stutter.

- [ ] **Step 6: Independent review and final commit**

Require separate spec and standards reviews with no open P0/P1/P2, then run `git diff --check` and `pnpm check` again.

```powershell
git add docs/development.md scripts/smoke-runtime.ps1
git commit -m "test: verify immersive skins on Windows"
```

---

## Self-Review

- Spec coverage: native picker, managed copy, four fit modes, nine positions, blur, light/dark mask, mask/panel opacity, enable/disable, live preview, read-only protocol, exact version adapter, DOM fallback, tray entry, persistence and performance each map to Tasks 1–8.
- Security coverage: official page receives no file/dialog/runtime/update command; its only remote command is a typed fail-safe compatibility report. Request text never becomes a filesystem path.
- Deletion coverage: no task deletes an image; reset changes settings only. Cleanup remains explicitly outside Phase 3.
- Placeholder scan: no red-flag placeholder or unspecified implementation step remains.
- Type consistency: `SkinStateEnvelope.revision`, `SkinDraft`, four command names, `appearance` label, protocol digest and adapter version are consistent across tasks.
