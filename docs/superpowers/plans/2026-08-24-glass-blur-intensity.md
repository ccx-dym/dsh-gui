# 毛玻璃强度设置 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为沉浸式皮肤增加默认 `0px`、范围 `0..=32px` 的独立毛玻璃强度，并让一张连续背景图后的中央区、侧栏、详情栏、输入卡片和标题栏使用同一模糊半径。

**Architecture:** 在现有皮肤设置数据流中增加 `glass_blur_px`，用严格 Schema 2 持久化并把 Schema 1 只读迁移为 `0`。前端草稿和外观预览保持原子保存语义；精确 DSH `0.1.1-rc.2` 适配器只在值大于零时生成玻璃 CSS，图片仍由现有唯一 `body::before` 层绘制。

**Tech Stack:** Rust 2024、Serde、Tauri 2、TypeScript、原生 DOM/CSS、Vitest/jsdom、Cargo test、Node 可执行脚本测试

**Spec:** `docs/superpowers/specs/2026-08-24-glass-blur-intensity-design.md`

## Global Constraints

- Python 相关脚本继续兼容 Python 3.10+；本功能不新增 Python 或生产依赖。
- `glass_blur_px` 是 `0..=32` 的整数，默认值与 Schema 1 迁移值均为 `0`。
- `blur_px` 继续只控制背景图片；`panel_opacity_percent` 继续只承载图片不透明度。
- 主窗口始终只绘制一张背景图，不在 DSH 面板中复制图片 URL。
- 五个验证区域使用完全相同的毛玻璃模糊半径。
- `0px` 不生成毛玻璃专用滤镜、高光或阴影。
- 仅支持经过精确验证的 DSH `0.1.1-rc.2` DOM/CSS 合约，未知版本继续失败关闭。
- 保持 API → Service → Repository 边界、严格输入校验、脱敏错误和 revision 原子保存语义。
- 不新增生产依赖，不改端口，不修改 runtime、更新链路或图片协议权限。
- 工作树已有 `src-tauri/src/skin/adapter.rs` 与 `src-tauri/tests/skin_adapter.rs` 的未提交实验改动；实施时在其上演进，不得 reset、checkout 或覆盖用户改动。

---

## File Structure

- `src-tauri/src/skin/model.rs`：Rust 领域模型与 Tauri 线协议字段。
- `src-tauri/src/skin/store.rs`：Schema 1/2 严格解析、迁移、范围校验和 Schema 2 写入。
- `src-tauri/tests/skin_store.rs`：迁移、边界、默认值和持久化回归。
- `src-tauri/src/skin/controller.rs`、`src-tauri/src/skin/protocol.rs`：更新现有 Rust 测试夹具中的完整草稿/设置结构。
- `src-tauri/tests/skin_protocol.rs`：更新序列化边界的精确 JSON 预期。
- `src/skin-state.ts`：TypeScript wire、草稿、reducer 与保存转换。
- `src/skin-state.test.ts`：前端范围收敛、事件同步与 payload 回归。
- `src/appearance.ts`：新增滑块、引用、事件和预览玻璃样式。
- `src/appearance.test.ts`：滑块可访问性、实时预览、零值与保存 payload。
- `src/main.test.ts`：补齐主界面测试夹具的完整 wire 字段。
- `src-tauri/src/skin/adapter.rs`：按设置值生成零值回退或统一玻璃 CSS。
- `src-tauri/tests/skin_adapter.rs`：执行真实生成脚本，验证零值、正值、统一半径和唯一图片层。
- `docs/用户使用指南.md`：解释背景模糊与毛玻璃强度的区别。

### Task 1: Rust 设置模型与 Schema 2 迁移

**Files:**
- Modify: `src-tauri/src/skin/model.rs`
- Modify: `src-tauri/src/skin/store.rs`
- Modify: `src-tauri/tests/skin_store.rs`
- Modify: `src-tauri/src/skin/controller.rs`
- Modify: `src-tauri/src/skin/protocol.rs`
- Modify: `src-tauri/tests/skin_protocol.rs`
- Modify: `src-tauri/tests/skin_adapter.rs`

**Interfaces:**
- Consumes: 现有 `SkinDraft -> SkinSettings` 转换、`SkinStore::{load,save,reset}` 和严格 Serde DTO。
- Produces: `SkinDraft::glass_blur_px: u8`、`SkinSettings::glass_blur_px: u8`；Schema 1 到 Schema 2 的只读迁移；所有新写入固定为 Schema 2。

- [ ] **Step 1: 写默认值、范围和迁移失败测试**

在 `src-tauri/tests/skin_store.rs` 先扩展夹具，并新增以下测试。Schema 1 夹具不能包含新字段；Schema 2 夹具必须包含新字段：

```rust
fn persisted_json(schema: u8, revision: u64, settings: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema": schema,
        "revision": revision,
        "settings": settings,
    }))
    .expect("应编码设置夹具")
}

fn legacy_settings_json() -> serde_json::Value {
    serde_json::json!({
        "immersive": false,
        "image_digest": null,
        "fit": "cover",
        "position": "center",
        "blur_px": 12,
        "mask_tone": "light",
        "mask_opacity_percent": 24,
        "panel_opacity_percent": 86,
    })
}

#[test]
fn schema_one_loads_in_memory_with_zero_glass_blur_and_saves_as_schema_two() {
    let (store, root) = fixture_store("schema-one-migration");
    let path = root.join("settings").join("skin.json");
    fs::write(&path, persisted_json(1, 7, legacy_settings_json()))
        .expect("应写入 schema 1 夹具");

    let loaded = store.load().expect("schema 1 应可迁移读取");
    assert_eq!(loaded.revision, 7);
    assert_eq!(loaded.settings.glass_blur_px, 0);
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap()["schema"], 1);

    let settings = loaded.settings;
    let saved = store.save(
        7,
        SkinDraft {
            immersive: settings.immersive,
            image_digest: settings.image_digest,
            fit: settings.fit,
            position: settings.position,
            blur_px: settings.blur_px,
            glass_blur_px: settings.glass_blur_px,
            mask_tone: settings.mask_tone,
            mask_opacity_percent: settings.mask_opacity_percent,
            panel_opacity_percent: settings.panel_opacity_percent,
        },
    ).expect("显式保存应升级 schema");
    assert_eq!(saved.settings.glass_blur_px, 0);
    let persisted: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(persisted["schema"], 2);
    assert_eq!(persisted["settings"]["glass_blur_px"], 0);
}

#[test]
fn glass_blur_accepts_closed_range_and_rejects_above_maximum() {
    let (store, _) = fixture_store("glass-range");
    let zero = store.save(0, SkinDraft { glass_blur_px: 0, ..valid_draft() }).unwrap();
    let maximum = store.save(zero.revision, SkinDraft { glass_blur_px: 32, ..valid_draft() }).unwrap();
    assert_eq!(maximum.settings.glass_blur_px, 32);
    assert_eq!(
        store.save(maximum.revision, SkinDraft { glass_blur_px: 33, ..valid_draft() })
            .expect_err("33 必须被拒绝")
            .kind(),
        SkinErrorKind::InvalidSettings,
    );
}
```

同步扩展现有 `defaults_are_non_immersive_and_bounded` 与 `reset_changes_only_the_pointer_and_keeps_imported_files`，分别断言默认值和恢复默认结果均为 `glass_blur_px == 0`。把 `invalid_values_loaded_from_disk_are_classified_as_corrupt_settings` 的替换集合增加以下三项，以覆盖 JSON 无法反序列化为 `u8` 以及合法 `u8` 越界：

```rust
serde_json::json!({ "glass_blur_px": -1 }),
serde_json::json!({ "glass_blur_px": 1.5 }),
serde_json::json!({ "glass_blur_px": 33 }),
```

- [ ] **Step 2: 运行定向测试确认 RED**

Run:

```powershell
cargo test --locked --test skin_store schema_one_loads_in_memory_with_zero_glass_blur_and_saves_as_schema_two -- --exact
cargo test --locked --test skin_store glass_blur_accepts_closed_range_and_rejects_above_maximum -- --exact
```

Expected: 编译失败或断言失败，原因是模型尚无 `glass_blur_px`、Schema 1 尚不能迁移且写入版本仍为 1；不能接受与目标无关的夹具语法错误。

- [ ] **Step 3: 给领域模型增加完整字段**

在 `SkinDraft` 和 `SkinSettings` 中增加字段，在默认值和转换中显式传递：

```rust
pub struct SkinDraft {
    pub immersive: bool,
    pub image_digest: Option<String>,
    pub fit: SkinFit,
    pub position: SkinPosition,
    pub blur_px: u8,
    /// DSH 内容表面对唯一背景图应用的毛玻璃模糊半径。
    pub glass_blur_px: u8,
    pub mask_tone: MaskTone,
    pub mask_opacity_percent: u8,
    /// schema 1 兼容字段；当前语义为背景图片不透明度百分比。
    pub panel_opacity_percent: u8,
}

pub struct SkinSettings {
    pub immersive: bool,
    pub image_digest: Option<String>,
    pub fit: SkinFit,
    pub position: SkinPosition,
    pub blur_px: u8,
    /// DSH 内容表面对唯一背景图应用的毛玻璃模糊半径。
    pub glass_blur_px: u8,
    pub mask_tone: MaskTone,
    pub mask_opacity_percent: u8,
    /// schema 1 兼容字段；当前语义为背景图片不透明度百分比。
    pub panel_opacity_percent: u8,
}
```

`Default` 必须写 `glass_blur_px: 0`，`From<SkinDraft> for SkinSettings` 必须复制该字段。更新 Rust 全部结构体字面量：普通夹具使用 `glass_blur_px: 0`；适配器正值夹具使用 `glass_blur_px: 16`。

- [ ] **Step 4: 实现严格双 Schema 读取与 Schema 2 写入**

在 `store.rs` 使用两个严格 DTO，不给 `SkinSettings` 加会掩盖 Schema 2 缺字段的 Serde 默认：

```rust
const SCHEMA_VERSION: u8 = 2;
const LEGACY_SCHEMA_VERSION: u8 = 1;
const MAX_GLASS_BLUR_PX: u8 = 32;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedSkinStateV1 {
    schema: u8,
    revision: u64,
    settings: SkinSettingsV1,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkinSettingsV1 {
    immersive: bool,
    image_digest: Option<String>,
    fit: SkinFit,
    position: SkinPosition,
    blur_px: u8,
    mask_tone: MaskTone,
    mask_opacity_percent: u8,
    panel_opacity_percent: u8,
}
```

先把有限字节解析成 `serde_json::Value`，只读取数值 `schema` 决定严格反序列化目标；Schema 1 转换时补 `glass_blur_px: 0`，Schema 2 使用现有 `PersistedSkinState`。未知 Schema、缺字段和未知字段统一映射为 `CorruptSettings`。核心分派写成：

```rust
let value: serde_json::Value =
    serde_json::from_slice(&bytes).map_err(|_| SkinError::CorruptSettings)?;
let schema = value
    .get("schema")
    .and_then(serde_json::Value::as_u64)
    .ok_or(SkinError::CorruptSettings)?;
let envelope = match schema {
    schema if schema == u64::from(LEGACY_SCHEMA_VERSION) => {
        let PersistedSkinStateV1 {
            schema,
            revision,
            settings,
        } = serde_json::from_value(value).map_err(|_| SkinError::CorruptSettings)?;
        debug_assert_eq!(schema, LEGACY_SCHEMA_VERSION);
        SkinStateEnvelope {
            revision,
            settings: SkinSettings {
                immersive: settings.immersive,
                image_digest: settings.image_digest,
                fit: settings.fit,
                position: settings.position,
                blur_px: settings.blur_px,
                glass_blur_px: 0,
                mask_tone: settings.mask_tone,
                mask_opacity_percent: settings.mask_opacity_percent,
                panel_opacity_percent: settings.panel_opacity_percent,
            },
        }
    }
    schema if schema == u64::from(SCHEMA_VERSION) => {
        let persisted: PersistedSkinState =
            serde_json::from_value(value).map_err(|_| SkinError::CorruptSettings)?;
        SkinStateEnvelope {
            revision: persisted.revision,
            settings: persisted.settings,
        }
    }
    _ => return Err(SkinError::CorruptSettings),
};
```

`persist` 始终写 `schema: 2`。

在 `validate_fields` 增加 `glass_blur_px` 参数并检查：

```rust
if blur_px > MAX_BLUR_PX
    || glass_blur_px > MAX_GLASS_BLUR_PX
    || mask_opacity_percent > MAX_MASK_OPACITY_PERCENT
    || panel_opacity_percent > MAX_IMAGE_OPACITY_PERCENT
    || image_digest.is_some_and(|digest| !is_canonical_digest(digest))
{
    return Err(SkinError::InvalidSettings);
}
```

`reset` 构造的 `SkinDraft` 必须从默认设置复制 `glass_blur_px`。

- [ ] **Step 5: 更新严格 JSON 与控制器夹具**

在 `src-tauri/tests/skin_protocol.rs` 的精确序列化预期中加入：

```text
"blur_px":0,"glass_blur_px":0,"mask_tone":"light"
```

在 `controller.rs`、`protocol.rs` 和 `skin_adapter.rs` 的测试结构体字面量补齐字段。扩展 `persisted_json_is_strict_and_round_trips_revision`：分别从 Schema 2 的 `settings` 删除 `glass_blur_px`、向 `settings` 插入 `unexpected: true` 后重新读取，两种情况都断言 `SkinErrorKind::CorruptSettings`。不要放宽 `deny_unknown_fields`，不要让 Schema 2 缺少 `glass_blur_px` 时静默使用默认值。

- [ ] **Step 6: 运行 Rust 皮肤数据层回归确认 GREEN**

Run:

```powershell
cargo test --locked --test skin_store
cargo test --locked --test skin_protocol
cargo test --locked skin::controller
cargo test --locked skin::protocol
```

Expected: 所有测试 PASS；Schema 1 文件只在显式保存后升级，Schema 2 严格拒绝缺字段、未知字段与 `33`。

- [ ] **Step 7: 提交 Rust 模型和迁移**

```powershell
git add src-tauri/src/skin/model.rs src-tauri/src/skin/store.rs src-tauri/tests/skin_store.rs src-tauri/src/skin/controller.rs src-tauri/src/skin/protocol.rs src-tauri/tests/skin_protocol.rs src-tauri/tests/skin_adapter.rs
git commit -m "feat: persist glass blur intensity"
```

### Task 2: TypeScript wire 与草稿状态

**Files:**
- Modify: `src/skin-state.ts`
- Modify: `src/skin-state.test.ts`
- Modify: `src/appearance.test.ts`
- Modify: `src/main.test.ts`

**Interfaces:**
- Consumes: Rust 序列化字段 `glass_blur_px: u8`。
- Produces: `SkinSettingsWire.glass_blur_px: number`、`SkinDraft.glassBlurPx: number` 和 `visuals.glassBlurPx?: number`。

- [ ] **Step 1: 写 wire、夹取和事件同步失败测试**

给 `savedEnvelope.settings` 增加 `glass_blur_px: 0`，并在现有范围测试中加入：

```typescript
const state = reduceSkinDraft(createInitialSkinState(savedEnvelope), {
  type: "visuals",
  glassBlurPx: 99.8,
});

expect(state.draft.glassBlurPx).toBe(32);
expect(skinDraftToWire(state.draft).glass_blur_px).toBe(32);
```

在“忽略倒序事件并用新 revision 同步”测试的新 envelope 中设置 `glass_blur_px: 12`，断言 `current.draft.glassBlurPx === 12`。

- [ ] **Step 2: 运行定向测试确认 RED**

Run:

```powershell
pnpm vitest run src/skin-state.test.ts
```

Expected: TypeScript 编译失败，指出 `glass_blur_px`、`glassBlurPx` 或 action 字段尚不存在。

- [ ] **Step 3: 实现完整前端状态传递**

在 `skin-state.ts` 增加：

```typescript
export interface SkinSettingsWire {
  immersive: boolean;
  image_digest: string | null;
  fit: SkinFit;
  position: SkinPosition;
  blur_px: number;
  glass_blur_px: number;
  mask_tone: MaskTone;
  mask_opacity_percent: number;
  panel_opacity_percent: number;
}

export interface SkinDraft {
  immersive: boolean;
  imageDigest: string | null;
  fit: SkinFit;
  position: SkinPosition;
  blurPx: number;
  glassBlurPx: number;
  maskTone: MaskTone;
  maskOpacityPercent: number;
  imageOpacityPercent: number;
}
```

`draftFromWire`、`skinDraftToWire` 和 `visuals` reducer 分支都用 `clampInteger(value, 0, 32)`。不得用 `blurPx` 派生毛玻璃值，也不得把字段映射到 `panel_opacity_percent`。

- [ ] **Step 4: 更新所有 TypeScript 完整 wire 夹具**

在 `appearance.test.ts` 和 `main.test.ts` 的 settings 字面量增加 `glass_blur_px: 0`。保持类型严格，不把字段改成可选。

- [ ] **Step 5: 运行前端状态测试确认 GREEN**

Run:

```powershell
pnpm vitest run src/skin-state.test.ts src/main.test.ts
pnpm exec tsc --noEmit
```

Expected: 全部 PASS，TypeScript 无缺失字段或隐式 `any`。

- [ ] **Step 6: 提交前端状态协议**

```powershell
git add src/skin-state.ts src/skin-state.test.ts src/appearance.test.ts src/main.test.ts
git commit -m "feat: carry glass blur through skin drafts"
```

### Task 3: 外观设置滑块与实时预览

**Files:**
- Modify: `src/appearance.ts`
- Modify: `src/appearance.test.ts`

**Interfaces:**
- Consumes: `SkinDraft.glassBlurPx` 与 `visuals.glassBlurPx`。
- Produces: `#skin-glass-blur` range、关联 output、保存 payload 和只作用于预览内容层的毛玻璃样式。

- [ ] **Step 1: 写滑块与零值预览失败测试**

在 `appearance.test.ts` 增加：

```typescript
it("毛玻璃强度实时作用于内容层且零值完整关闭玻璃装饰", async () => {
  const root = document.querySelector<HTMLElement>("#app")!;
  const dispose = await initializeAppearance(root, bridge());
  const slider = root.querySelector<HTMLInputElement>("#skin-glass-blur")!;
  const background = root.querySelector<HTMLElement>("[data-skin-background]")!;
  const content = root.querySelector<HTMLElement>("[data-skin-preview-content]")!;

  expect(root.querySelector("label[for='skin-glass-blur']")?.textContent).toBe("毛玻璃强度");
  expect(slider.min).toBe("0");
  expect(slider.max).toBe("32");
  expect(slider.value).toBe("0");
  expect(content.style.backdropFilter).toBe("none");

  slider.value = "16";
  slider.dispatchEvent(new Event("input", { bubbles: true }));
  expect(content.style.backdropFilter).toBe("blur(16px) saturate(1.28)");
  expect(content.style.webkitBackdropFilter).toBe("blur(16px) saturate(1.28)");
  expect(background.style.filter).toBe("blur(12px)");

  slider.value = "0";
  slider.dispatchEvent(new Event("input", { bubbles: true }));
  expect(content.style.backdropFilter).toBe("none");
  expect(content.style.boxShadow).toBe("none");
  dispose();
});
```

同时把保存测试扩展为 `draft: expect.objectContaining({ glass_blur_px: 0 })`。

- [ ] **Step 2: 运行定向测试确认 RED**

Run:

```powershell
pnpm vitest run src/appearance.test.ts
```

Expected: FAIL，原因是找不到 `#skin-glass-blur` 或内容层没有对应样式；不能因 envelope 缺字段而失败。

- [ ] **Step 3: 增加稳定 DOM 引用与滑块**

在 `AppearanceViewRefs` 增加：

```typescript
glassBlur: HTMLInputElement;
glassBlurOutput: HTMLOutputElement;
```

在“背景模糊”之后创建：

```typescript
createSlider(
  "skin-glass-blur",
  "毛玻璃强度",
  state.draft.glassBlurPx,
  0,
  32,
  "glass-blur",
)
```

在 `onInput` 中把 `data-control="glass-blur"` 映射为 `{ type: "visuals", glassBlurPx: value }`。沿用现有 WeakMap patch 模式，不在 slider 输入时重建 DOM。

- [ ] **Step 4: 实现预览内容层样式切换**

在 `patchAppearanceView` 中只修改 `refs.content`：

```typescript
const glassBlur = state.draft.glassBlurPx;
const glassFilter = `blur(${glassBlur}px) saturate(1.28)`;
refs.content.style.backdropFilter = glassBlur === 0 ? "none" : glassFilter;
refs.content.style.webkitBackdropFilter = glassBlur === 0 ? "none" : glassFilter;
refs.content.style.background = glassBlur === 0 ? "transparent" : "rgb(255 255 255 / 36%)";
refs.content.style.boxShadow = glassBlur === 0
  ? "none"
  : "inset 0 1px 0 rgb(255 255 255 / 20%), 0 18px 48px rgb(0 0 0 / 18%)";
```

深色 tone 使用 `rgb(22 28 38 / 36%)`；提取一个内部纯函数根据 tone 返回背景色即可，但不要引入新模块或依赖。同步 patch slider value/output 为 `Npx`。

- [ ] **Step 5: 运行外观测试确认 GREEN**

Run:

```powershell
pnpm vitest run src/appearance.test.ts src/skin-state.test.ts
pnpm exec tsc --noEmit
```

Expected: PASS；背景节点仍只有一个，背景模糊值在移动毛玻璃滑块后不变，滑块节点不被重建。

- [ ] **Step 6: 提交设置页交互**

```powershell
git add src/appearance.ts src/appearance.test.ts
git commit -m "feat: add glass blur appearance control"
```

### Task 4: DSH 适配器按强度生成统一玻璃层

**Files:**
- Modify: `src-tauri/src/skin/adapter.rs`
- Modify: `src-tauri/tests/skin_adapter.rs`

**Interfaces:**
- Consumes: 已验证 `SkinSettings::glass_blur_px`。
- Produces: `0px` 官方透明皮肤回退；正值时五个精确区域共享相同 `blur(Npx)`，背景图仍只出现一次。

- [ ] **Step 1: 把现有实验测试拆成零值与统一正值测试**

保留现有 Node harness，但用实际执行后插入的 `style.textContent` 断言用户可见行为：

```rust
#[test]
fn zero_glass_blur_keeps_one_clear_wallpaper_without_glass_decorations() {
    let mut settings = fixture_settings();
    settings.glass_blur_px = 0;
    let css = execute_style_text(&settings);

    assert_eq!(css.matches("http://dsh-skin.localhost/").count(), 1);
    assert!(!css.contains("backdrop-filter"));
    assert!(!css.contains(".pI_x6G_centerCol{"));
    assert!(!css.contains("box-shadow:inset"));
}

#[test]
fn positive_glass_blur_uses_one_radius_for_every_verified_surface() {
    for radius in [1, 16, 32] {
        let mut settings = fixture_settings();
        settings.glass_blur_px = radius;
        let css = execute_style_text(&settings);

        for selector in [
            ".pI_x6G_centerCol",
            ".pI_x6G_sidebarCol",
            ".pI_x6G_detailsCol",
            "[data-composer-card]",
            "#dsh-desktop-titlebar",
        ] {
            assert!(css.contains(selector));
        }
        let filter = format!("blur({radius}px) saturate(1.28)");
        assert_eq!(css.matches(&filter).count(), 2);
        assert_eq!(css.matches("http://dsh-skin.localhost/").count(), 1);
    }
}
```

`execute_style_text` 放在测试文件中，复用当前 Node harness；它接收 `&SkinSettings`，调用真实 `adapter_script` 并返回插入的 CSS。不要把测试辅助函数放进生产模块：

```rust
fn execute_style_text(settings: &SkinSettings) -> String {
    let script = adapter_script(settings).expect("沉浸皮肤应生成脚本");
    let harness = format!(
        r#"const inserted=[];
const root={{}};
global.requestAnimationFrame=(callback)=>{{callback();return 1;}};
global.document={{
  getElementById:()=>null,
  querySelector:()=>root,
  createElement:(tag)=>({{id:'',style:{{cssText:''}},setAttribute:()=>{{}},textContent:'',tag}}),
  documentElement:{{prepend:(node)=>inserted.push(node)}},
  head:{{append:(node)=>inserted.push(node)}}
}};
global.getComputedStyle=()=>({{getPropertyValue:()=> '#151517'}});
global.location={{protocol:'http:',hostname:'127.0.0.1',port:'43127'}};
global.__TAURI_INTERNALS__={{invoke:()=>Promise.resolve()}};
{script}
const style=inserted.find((node)=>node.tag==='style');
console.log(style?.textContent??'NO_STYLE');"#
    );
    let output = Command::new("node")
        .args(["-e", &harness])
        .output()
        .expect("前端工具链必须提供 node");
    assert!(
        output.status.success(),
        "Node harness 执行失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("CSS 输出必须是 UTF-8")
        .trim()
        .to_owned()
}
```

- [ ] **Step 2: 运行定向测试确认 RED**

Run:

```powershell
cargo test --locked --test skin_adapter zero_glass_blur_keeps_one_clear_wallpaper_without_glass_decorations -- --exact
cargo test --locked --test skin_adapter positive_glass_blur_uses_one_radius_for_every_verified_surface -- --exact
```

Expected: 第一个测试因现有硬编码玻璃 CSS 在零值时仍存在而失败；第二个测试因中央区缺失且存在硬编码 `22px/24px` 而失败。

- [ ] **Step 3: 生成零值回退与统一正值 CSS**

在 `adapter_script_for_page` 中用显式分支生成完整表面样式。零值精确沿用实验前的可读层级；正值只把已校验整数格式化为 CSS，并让五个选择器共享同一滤镜：

```rust
let glass_surfaces = if settings.glass_blur_px == 0 {
    format!(
        r#":root,#root{{--dsw-alias-bg-base:transparent !important;--dsw-alias-bg-layer-1:rgba({surface_rgb},0.88) !important;--dsw-alias-bg-layer-2:rgba({surface_rgb},0.88) !important;--dsw-specific-sidebar-fill:transparent !important;--dsh-desktop-border-opacity:0.88 !important}}[data-composer-card]{{background:transparent !important}}"#,
    )
} else {
    let glass_blur_px = settings.glass_blur_px;
    let glass_filter = format!("blur({glass_blur_px}px) saturate(1.28)");
    format!(
        r#":root,#root{{--dsw-alias-bg-base:transparent !important;--dsw-alias-bg-layer-1:rgba({surface_rgb},0.50) !important;--dsw-alias-bg-layer-2:rgba({surface_rgb},0.74) !important;--dsw-specific-sidebar-fill:rgba({surface_rgb},0.18) !important;--dsh-desktop-border-opacity:0.58 !important}}.pI_x6G_centerCol,.pI_x6G_sidebarCol,.pI_x6G_detailsCol,[data-composer-card],#dsh-desktop-titlebar{{backdrop-filter:{glass_filter};-webkit-backdrop-filter:{glass_filter}}}.pI_x6G_centerCol{{background:rgba({surface_rgb},0.20) !important}}.pI_x6G_sidebarCol{{background:rgba({surface_rgb},0.30) !important;box-shadow:inset -1px 0 0 rgba(255,255,255,0.18),12px 0 36px rgba(0,0,0,0.12)}}.pI_x6G_detailsCol{{background:rgba({surface_rgb},0.34) !important;box-shadow:inset 1px 0 0 rgba(255,255,255,0.16),-12px 0 36px rgba(0,0,0,0.10)}}[data-composer-card]{{background:rgba({surface_rgb},0.36) !important;border-color:rgba(255,255,255,0.56) !important;box-shadow:inset 0 1px 0 rgba(255,255,255,0.20),0 18px 48px rgba(0,0,0,0.18)}}#dsh-desktop-titlebar{{background:rgba({surface_rgb},0.24);border-bottom:1px solid rgba(255,255,255,0.42);box-shadow:inset 0 1px 0 rgba(255,255,255,0.16),0 10px 30px rgba(0,0,0,0.12)}}"#,
    )
};
```

零值分支不得输出 `backdrop-filter`、`box-shadow` 或玻璃边缘 border。中央区使用固定低染色，侧栏、详情栏和输入卡片使用固定但不同的半透明背景；只允许透明度不同，不允许模糊半径不同。

- [ ] **Step 4: 保留安全门禁并移除实验硬编码**

检查最终 diff，保留现有 `body::before` 唯一图片层、`body::after` 遮罩、来源门禁、精确版本门禁和 DOM 合约门禁。删除实验代码中的硬编码 `22px`、`24px`，不得扩展选择器集合或生成第二个背景 URL。

- [ ] **Step 5: 运行适配器回归确认 GREEN**

Run:

```powershell
cargo test --locked --test skin_adapter
```

Expected: 全部 PASS；零值无玻璃装饰，`1`、`16`、`32` 均不越界且五个区域半径一致，脚本仍无 fetch、storage、timer 或业务事件监听。

- [ ] **Step 6: 提交运行时渲染**

```powershell
git add src-tauri/src/skin/adapter.rs src-tauri/tests/skin_adapter.rs
git commit -m "feat: apply configurable glass blur to DSH"
```

### Task 5: 用户文档与完整自动化验证

**Files:**
- Modify: `docs/用户使用指南.md`
- Verify: all modified source and test files

**Interfaces:**
- Consumes: 已完成的 Schema 2、外观滑块和 DSH 适配器。
- Produces: 用户可操作说明、无格式问题的完整构建和可供实际验收的生产版程序。

- [ ] **Step 1: 更新用户指南**

把外观设置步骤改为明确区分两个参数：

```markdown
3. 调整填充方式、位置、背景模糊、毛玻璃强度、遮罩和图片不透明度。
   “背景模糊”只处理图片本身；“毛玻璃强度”控制主界面内容层，范围为
   `0px` 至 `32px`，`0px` 表示关闭毛玻璃。图片不透明度 `0%` 会隐藏背景图片，
   但不会改变遮罩或正文。
```

补充说明主窗口仍只使用一张连续背景图。

- [ ] **Step 2: 运行完整前端验证**

Run:

```powershell
pnpm test
pnpm build
```

Expected: Vitest 0 failures；`tsc --noEmit` 和 Vite production build exit 0。

- [ ] **Step 3: 运行完整 Rust 验证**

Run:

```powershell
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Expected: 格式检查 exit 0；全部非 ignored 测试通过；Clippy 无 warning/error。现有需要真实公开通道的 ignored 测试可继续 ignored。

- [ ] **Step 4: 检查差异与安全边界**

Run:

```powershell
git diff --check
rg -n "blur\(22px\)|blur\(24px\)|TO[D]O|TB[D]" src src-tauri docs/用户使用指南.md
rg -n "http://dsh-skin\.localhost" src-tauri/src/skin/adapter.rs
git status --short
```

Expected: 无 whitespace error、无遗留硬编码玻璃半径或占位符；适配器只有现有唯一图片 URL 构造；状态只包含本计划文件和预期源文件改动。

- [ ] **Step 5: 构建生产版程序**

确保正在运行的预览版已由用户明确退出或经只读检查确认没有 DSH 子进程/监听端口后再停止；随后运行：

```powershell
pnpm tauri build --no-bundle
```

Expected: exit 0，产物为 `src-tauri/target/release/dsh-desktop.exe`。若 Tauri CLI 把 `tauri-build = "=2.6.3"` 机械展开为等价 table 写法，使用 `apply_patch` 恢复原写法，不把无关 manifest 改动提交。

- [ ] **Step 6: 执行 Windows WebView2 实际验收**

启动新生产版，使用同一张有明显细节的背景图依次保存并检查：

```text
0px  → 标题栏、侧栏、中央区、详情栏和输入卡片无玻璃模糊/高光，背景清晰连续
16px → 五个区域模糊程度一致，背景仍可辨认，没有图片接缝或重复
32px → 五个区域显著模糊但文字、审批、菜单和输入控件仍清晰可用
```

每次保存后确认诊断出现 `skin_dom_compatible`，并检查首页、已有会话、侧栏收起/展开、详情栏开关。不得为了验收改写用户背景图片或删除现有设置。

- [ ] **Step 7: 提交文档与验证收尾**

```powershell
git add docs/用户使用指南.md
git commit -m "docs: explain glass blur control"
git status --short
```

Expected: 用户指南提交成功；工作树不包含意外依赖、端口、runtime、安装数据或生成物改动。
