# Skin Image Opacity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the ineffective panel-opacity control with real background-image opacity in the appearance preview and main chat window.

**Architecture:** Preserve the legacy `panel_opacity_percent` wire/storage key for backward compatibility while renaming its TypeScript domain meaning to image opacity. Render the wallpaper on its own layer so opacity affects only the image; keep the readability mask and transparent DSH chrome independent.

**Tech Stack:** TypeScript 7, Vitest/JSDOM, Rust, Tauri 2, generated CSS adapter

**Spec:** `docs/superpowers/specs/2026-08-24-skin-image-opacity-design.md`

## Global Constraints

- Make only the minimum behavior change; do not add dependencies or refactor unrelated code.
- Keep the existing `panel_opacity_percent` serialized key readable and writable.
- Image opacity is an integer from `0` through `100`.
- Mask opacity remains independent and the composer remains transparent.
- Execute inline in this task; do not dispatch subagents.

---

### Task 1: Frontend state and appearance preview

**Files:**
- Modify: `src/skin-state.test.ts`
- Modify: `src/appearance.test.ts`
- Modify: `src/skin-state.ts`
- Modify: `src/appearance.ts`
- Modify: `src/appearance.css`

**Interfaces:**
- Consumes: `SkinSettingsWire.panel_opacity_percent` as the legacy persisted key.
- Produces: `SkinDraft.imageOpacityPercent: number` and `{ type: "visuals"; imageOpacityPercent?: number }`.

- [x] **Step 1: Write failing reducer tests**

Change the reducer test to dispatch `imageOpacityPercent: -1` and assert `0`, then dispatch `101` and assert `100`. Assert `skinDraftToWire` still emits the legacy key with the chosen value.

- [x] **Step 2: Run the reducer test and verify RED**

Run: `pnpm test -- src/skin-state.test.ts`

Expected: TypeScript/test failure because `imageOpacityPercent` does not exist and the old minimum is `55`.

- [x] **Step 3: Implement the minimal state mapping**

Rename the TypeScript domain property/action to `imageOpacityPercent`, map it to/from `panel_opacity_percent`, and clamp it to `0..100`.

- [x] **Step 4: Run the reducer test and verify GREEN**

Run: `pnpm test -- src/skin-state.test.ts`

Expected: all reducer tests pass.

- [x] **Step 5: Write failing appearance DOM tests**

Assert the slider is labelled `图片不透明度`, has `min="0"`, and changing it to `37` sets only the background image node opacity to `0.37` while the preview content opacity remains unchanged.

- [x] **Step 6: Run the appearance test and verify RED**

Run: `pnpm test -- src/appearance.test.ts`

Expected: failure because the old label/range and panel preview behavior remain.

- [x] **Step 7: Implement the minimal preview behavior**

Update the slider copy/range/control mapping, set the preview image node opacity from the draft, and give the preview content a stable readable background independent of image opacity.

- [x] **Step 8: Run frontend tests and verify GREEN**

Run: `pnpm test -- src/skin-state.test.ts src/appearance.test.ts`

Expected: both suites pass.

### Task 2: Rust validation and main-window adapter

**Files:**
- Modify: `src-tauri/tests/skin_store.rs`
- Modify: `src-tauri/tests/skin_adapter.rs`
- Modify: `src-tauri/src/skin/store.rs`
- Modify: `src-tauri/src/skin/adapter.rs`
- Modify: `src-tauri/src/skin/model.rs`

**Interfaces:**
- Consumes: validated `SkinSettings.panel_opacity_percent` legacy field.
- Produces: generated CSS with an independent wallpaper layer at the requested opacity.

- [x] **Step 1: Write failing Rust boundary tests**

Update store fixtures to accept `0` and reject `101`. Add an adapter test setting the legacy field to `37` and executing the generated script to assert the inserted CSS gives the wallpaper layer opacity `0.37`, while DSH layer variables are no longer driven by that value.

- [x] **Step 2: Run focused Rust tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --locked --test skin_store --test skin_adapter`

Expected: store rejects `0`, and adapter does not emit the independent image-opacity layer.

- [x] **Step 3: Implement validation and adapter CSS**

Change validation to `0..=100`. Always render the image through `body::before`, apply opacity and optional blur to that layer, keep mask rendering independent, and stop using the value for DSH panel background variables.

- [x] **Step 4: Run focused Rust tests and verify GREEN**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --locked --test skin_store --test skin_adapter`

Expected: both integration test targets pass.

### Task 3: Documentation and full verification

**Files:**
- Modify: `docs/用户使用指南.md`

**Interfaces:**
- Consumes: the final user-visible label and `0..100` behavior.
- Produces: user documentation matching the application.

- [x] **Step 1: Update user documentation**

Replace the panel-opacity description with image-opacity semantics and state that masking remains independent.

- [x] **Step 2: Run formatting and full verification**

Run: `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`

Run: `cargo test --manifest-path src-tauri/Cargo.toml --locked`

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets -- -D warnings`

Run: `pnpm test`

Run: `pnpm build`

Expected: every command exits `0` with no test failures or compile errors.

- [x] **Step 3: Review the diff against the spec**

Confirm the legacy key is preserved, only the image fades, mask and composer behavior are unchanged, and no unrelated files changed.
