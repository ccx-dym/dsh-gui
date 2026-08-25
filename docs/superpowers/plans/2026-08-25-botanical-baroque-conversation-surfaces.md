# Botanical Baroque Conversation Surfaces Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为沉浸式皮肤的输入卡片和用户消息气泡增加固定、互不遮挡的 C×H 植物系巴洛克边框。

**Architecture:** 保留现有 Rust 适配器生成的唯一 `<style>`，先把输入卡片与用户气泡从共享完整声明拆成两个私有 CSS 生成单元，再用固定内嵌 SVG 数据 URI 和 `::before` 伪元素添加装饰。装饰不新增 DOM、设置字段、资源协议或网络访问；现有适配器门禁和清理脚本继续负责启用与完整回退。

**Tech Stack:** Rust 2024、Tauri 2、CSS 伪元素、内嵌 SVG data URI、Node 测试 harness、Cargo test/clippy、Vitest/Vite

**Spec:** `docs/superpowers/specs/2026-08-25-botanical-baroque-conversation-surfaces-design.md`

## Global Constraints

- 只装饰 `[data-composer-card]` 和 `[data-chat-flow-kind="user"] [data-slot="conversation.message.images"] + div`。
- 不修改皮肤设置 Schema、设置界面、存储迁移、背景图协议或适配器版本门禁。
- 不新增生产依赖、网络请求、云端服务、人物抠图、颜色分析或持续动画。
- 输入卡片与气泡继续共享 `conversation_surface_opacity_percent`，但分别维护边框、圆角、阴影和装饰声明。
- SVG 必须是编译期固定内容，不包含脚本、外链、字体、用户文本或远程资源。
- 装饰必须 `pointer-events: none`，不得覆盖正文、输入内容、占位文字或控件。
- 窄窗口隐藏输入卡片中央卷草和底边点珠，保留两个角花；气泡保持精简角花。
- 关闭皮肤、来源不可信、版本不兼容或 DOM 合约失败时继续使用现有失败关闭与清理流程。

## File Structure

- Modify: `src-tauri/src/skin/adapter.rs` — 定义固定选择器与 SVG 数据 URI，分别生成输入卡片、用户气泡和响应式装饰 CSS，并把结果接入现有皮肤脚本。
- Modify: `src-tauri/tests/skin_adapter.rs` — 从可执行脚本提取最终 CSS，锁定独立表面、固定 SVG、交互穿透、窄屏降级、透明度、玻璃强度和安全回退。

---

### Task 1: 拆分输入卡片与用户气泡基础表面

**Files:**
- Modify: `src-tauri/src/skin/adapter.rs:13-15,144-164`
- Test: `src-tauri/tests/skin_adapter.rs:287-294`

**Interfaces:**
- Consumes: `surface_rgb: &str` 与 `conversation_surface_opacity: f32`，均来自已经验证的 `SkinSettings`。
- Produces: `fn conversation_surface_css(surface_rgb: &str, opacity: f32) -> String`，供 `adapter_script_for_page` 的零毛玻璃和正毛玻璃分支共同复用。

- [ ] **Step 1: 把共享透明度测试改为同时锁定独立造型**

在 `src-tauri/tests/skin_adapter.rs` 中用以下测试替换 `composer_and_user_messages_share_the_conversation_surface_opacity`：

```rust
#[test]
fn composer_and_user_messages_share_opacity_but_keep_independent_shapes() {
    let css = execute_style_text(&fixture_settings());

    assert!(css.contains(
        "[data-composer-card]{position:relative;isolation:isolate;overflow:visible !important;background:rgba(255,255,255,0.85)"
    ));
    assert!(css.contains("border-radius:22px !important"));
    assert!(css.contains(
        "[data-chat-flow-kind=\"user\"] [data-slot=\"conversation.message.images\"]+div{position:relative;isolation:isolate;overflow:visible !important;background:rgba(255,255,255,0.85)"
    ));
    assert!(css.contains("border-radius:18px 18px 6px 18px !important"));
    assert!(css.contains("0 18px 48px rgba(0,0,0,0.18)"));
    assert!(css.contains("0 10px 28px rgba(0,0,0,0.14)"));
}
```

同时把 `script_checks_dom_before_painting_the_page_canvas` 中旧输入卡片断言：

```rust
assert!(script.contains("[data-composer-card]{background:rgba(255,255,255,0.85)"));
```

替换为：

```rust
assert!(script.contains(
    "[data-composer-card]{position:relative;isolation:isolate;overflow:visible !important;background:rgba(255,255,255,0.85)"
));
```

- [ ] **Step 2: 运行定向测试并确认它因缺少独立声明失败**

Run:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --locked --test skin_adapter composer_and_user_messages_share_opacity_but_keep_independent_shapes -- --exact --nocapture
```

Expected: FAIL，首个缺失片段包含 `position:relative;isolation:isolate` 或独立圆角值。

- [ ] **Step 3: 添加固定选择器和基础表面生成函数**

在 `BACKGROUND_ID` 后增加：

```rust
const COMPOSER_SELECTOR: &str = "[data-composer-card]";
const USER_MESSAGE_SELECTOR: &str =
    r#"[data-chat-flow-kind="user"] [data-slot="conversation.message.images"]+div"#;
```

在 `adapter_script_for_page` 前增加私有函数：

```rust
fn conversation_surface_css(surface_rgb: &str, opacity: f32) -> String {
    // 输入卡片是持续交互的操作面，使用更完整的轮廓和阴影；历史气泡降低悬浮层级，
    // 两者只共享用户可调的不透明度，避免再次耦合完整视觉声明。
    format!(
        r#"{composer}{{position:relative;isolation:isolate;overflow:visible !important;background:rgba({surface_rgb},{opacity:.2}) !important;border:1px solid rgba(255,211,151,0.72) !important;border-radius:22px !important;box-shadow:inset 0 1px 0 rgba(255,255,255,0.20),0 18px 48px rgba(0,0,0,0.18),0 0 18px rgba(255,211,151,0.08)}}{message}{{position:relative;isolation:isolate;overflow:visible !important;background:rgba({surface_rgb},{opacity:.2}) !important;border:1px solid rgba(255,211,151,0.62) !important;border-radius:18px 18px 6px 18px !important;box-shadow:inset 0 1px 0 rgba(255,255,255,0.16),0 10px 28px rgba(0,0,0,0.14)}}"#,
        composer = COMPOSER_SELECTOR,
        message = USER_MESSAGE_SELECTOR,
    )
}
```

在 `adapter_script_for_page` 中删除原 `conversation_surfaces = format!(...)`，改为：

```rust
let conversation_surfaces =
    conversation_surface_css(surface_rgb, conversation_surface_opacity);
```

保留零毛玻璃和正毛玻璃分支中现有的 `{conversation_surfaces}` 插入位置，不复制该函数输出。

- [ ] **Step 4: 格式化并运行定向测试**

Run:

```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml --locked --test skin_adapter composer_and_user_messages_share_opacity_but_keep_independent_shapes -- --exact --nocapture
```

Expected: PASS。

- [ ] **Step 5: 运行当前适配器测试并修正旧精确断言**

Run:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --locked --test skin_adapter -- --nocapture
```

Expected: PASS；来源、版本、清理和脚本安全断言保持原样。

- [ ] **Step 6: 提交独立基础表面**

```powershell
git add -- src-tauri/src/skin/adapter.rs src-tauri/tests/skin_adapter.rs
git commit -m "refactor: separate conversation surface styles"
```

---

### Task 2: 添加固定植物系巴洛克 SVG 边框

**Files:**
- Modify: `src-tauri/src/skin/adapter.rs:13-18,96-170`
- Test: `src-tauri/tests/skin_adapter.rs:287-330`

**Interfaces:**
- Consumes: Task 1 的 `COMPOSER_SELECTOR`、`USER_MESSAGE_SELECTOR` 和 `conversation_surface_css(surface_rgb, opacity)`。
- Produces: 六个编译期固定 SVG data URI 常量，以及 `fn conversation_decoration_css() -> String`；`conversation_surface_css` 把装饰 CSS 追加到两个基础表面之后。

- [ ] **Step 1: 添加输入卡片与气泡装饰的失败测试**

在 Task 1 的独立造型测试后增加：

```rust
#[test]
fn botanical_baroque_decorations_use_fixed_noninteractive_svg_layers() {
    let css = execute_style_text(&fixture_settings());

    assert!(css.contains("[data-composer-card]::before{content:\"\";position:absolute"));
    assert!(css.contains(
        "[data-chat-flow-kind=\"user\"] [data-slot=\"conversation.message.images\"]+div::before{content:\"\";position:absolute"
    ));
    assert_eq!(css.matches("data:image/svg+xml").count(), 6);
    assert_eq!(css.matches("pointer-events:none").count(), 4);
    assert!(css.contains("background-repeat:no-repeat"));
    assert!(css.contains("rgba(255,211,151,0.72)"));
    assert!(!css.contains("<script"));
    assert!(!css.contains("javascript:"));
}
```

`pointer-events:none` 的四处来源固定为 `body::before`、`body::after`、输入卡片 `::before` 和用户气泡 `::before`。

- [ ] **Step 2: 运行测试并确认缺少 SVG 装饰**

Run:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --locked --test skin_adapter botanical_baroque_decorations_use_fixed_noninteractive_svg_layers -- --exact --nocapture
```

Expected: FAIL，缺少 `[data-composer-card]::before` 或 `data:image/svg+xml`。

- [ ] **Step 3: 添加六个固定 SVG 数据 URI**

在选择器常量后增加以下只包含路径、固定色和固定视图框的数据 URI：

```rust
const COMPOSER_TOP_LEFT_VINE_SVG: &str = concat!(
    "data:image/svg+xml,",
    "%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 72 64'%3E",
    "%3Cpath d='M3 62C9 43 19 31 37 25C49 21 58 13 64 3' fill='none' stroke='%23A9DDA4' stroke-width='2' stroke-linecap='round'/%3E",
    "%3Cpath d='M14 45C7 42 7 35 9 30C16 31 20 36 14 45ZM35 26C29 20 32 14 37 11C42 16 42 22 35 26ZM51 17C48 10 53 6 59 5C61 11 58 16 51 17Z' fill='%23A9DDA4' fill-opacity='.82'/%3E",
    "%3Ccircle cx='25' cy='34' r='5' fill='%23F3ABD3'/%3E%3Ccircle cx='25' cy='34' r='2' fill='%23FFD397'/%3E%3C/svg%3E"
);
const COMPOSER_CREST_SVG: &str = concat!(
    "data:image/svg+xml,",
    "%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 124 24'%3E",
    "%3Cpath d='M2 18C24 18 27 6 48 10C57 12 58 4 62 3C66 4 67 12 76 10C97 6 100 18 122 18' fill='none' stroke='%23FFD397' stroke-width='1.5' stroke-linecap='round'/%3E",
    "%3Cpath d='M54 12L62 5L70 12L62 19Z' fill='%23F3ABD3' fill-opacity='.82' stroke='%23FFD397'/%3E%3C/svg%3E"
);
const COMPOSER_BOTTOM_RIGHT_VINE_SVG: &str = concat!(
    "data:image/svg+xml,",
    "%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 72 64'%3E",
    "%3Cg transform='translate(72 64) rotate(180)'%3E%3Cpath d='M3 62C9 43 19 31 37 25C49 21 58 13 64 3' fill='none' stroke='%23A9DDA4' stroke-width='2' stroke-linecap='round'/%3E",
    "%3Cpath d='M14 45C7 42 7 35 9 30C16 31 20 36 14 45ZM35 26C29 20 32 14 37 11C42 16 42 22 35 26ZM51 17C48 10 53 6 59 5C61 11 58 16 51 17Z' fill='%23A9DDA4' fill-opacity='.82'/%3E",
    "%3Ccircle cx='25' cy='34' r='5' fill='%23F3ABD3'/%3E%3Ccircle cx='25' cy='34' r='2' fill='%23FFD397'/%3E%3C/g%3E%3C/svg%3E"
);
const COMPOSER_BEADS_SVG: &str = concat!(
    "data:image/svg+xml,",
    "%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 156 8'%3E",
    "%3Cpath d='M2 3H154' stroke='%23FFD397' stroke-opacity='.55'/%3E",
    "%3Cg fill='%23F3ABD3'%3E%3Ccircle cx='42' cy='3' r='2'/%3E%3Ccircle cx='58' cy='3' r='2'/%3E%3Ccircle cx='74' cy='3' r='2'/%3E%3Ccircle cx='90' cy='3' r='2'/%3E%3Ccircle cx='106' cy='3' r='2'/%3E%3C/svg%3E"
);
const MESSAGE_TOP_RIGHT_SPRIG_SVG: &str = concat!(
    "data:image/svg+xml,",
    "%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 58 42'%3E",
    "%3Cpath d='M2 7C18 8 25 17 32 27C37 34 46 38 56 40' fill='none' stroke='%23A9DDA4' stroke-width='2' stroke-linecap='round'/%3E",
    "%3Cpath d='M17 11C14 5 18 2 23 2C25 7 23 11 17 11ZM35 28C36 21 42 20 47 23C46 29 42 32 35 28Z' fill='%23A9DDA4' fill-opacity='.82'/%3E",
    "%3Ccircle cx='29' cy='21' r='4.5' fill='%23F3ABD3'/%3E%3Ccircle cx='29' cy='21' r='1.8' fill='%23FFD397'/%3E%3C/svg%3E"
);
const MESSAGE_BOTTOM_LEFT_FLOURISH_SVG: &str = concat!(
    "data:image/svg+xml,",
    "%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 48 28'%3E",
    "%3Cpath d='M2 25C12 24 15 15 21 14C28 13 27 22 20 22C12 22 13 7 27 5C36 4 39 10 46 3' fill='none' stroke='%23FFD397' stroke-width='1.5' stroke-linecap='round'/%3E",
    "%3Cpath d='M27 5C29 10 35 12 39 9' fill='none' stroke='%23A9DDA4' stroke-width='1.5'/%3E%3C/svg%3E"
);
```

- [ ] **Step 4: 生成两个伪元素的多重背景规则**

在 `conversation_surface_css` 前增加：

```rust
fn conversation_decoration_css() -> String {
    // 每个图层保持独立尺寸和锚点；中心透明，业务内容仍由真实 DOM 绘制。
    format!(
        r#"{composer}::before{{content:"";position:absolute;inset:-3px;z-index:0;pointer-events:none;border-radius:inherit;background-image:url("{top_left}"),url("{crest}"),url("{bottom_right}"),url("{beads}");background-position:left top,center top,right bottom,center bottom;background-size:72px 64px,124px 24px,72px 64px,156px 8px;background-repeat:no-repeat}}{message}::before{{content:"";position:absolute;inset:-2px;z-index:0;pointer-events:none;border-radius:inherit;background-image:url("{sprig}"),url("{flourish}");background-position:right top,left bottom;background-size:58px 42px,48px 28px;background-repeat:no-repeat}}"#,
        composer = COMPOSER_SELECTOR,
        message = USER_MESSAGE_SELECTOR,
        top_left = COMPOSER_TOP_LEFT_VINE_SVG,
        crest = COMPOSER_CREST_SVG,
        bottom_right = COMPOSER_BOTTOM_RIGHT_VINE_SVG,
        beads = COMPOSER_BEADS_SVG,
        sprig = MESSAGE_TOP_RIGHT_SPRIG_SVG,
        flourish = MESSAGE_BOTTOM_LEFT_FLOURISH_SVG,
    )
}
```

把 `conversation_surface_css` 的返回表达式改为先生成两个基础表面，再追加装饰：

```rust
fn conversation_surface_css(surface_rgb: &str, opacity: f32) -> String {
    let surfaces = format!(
        r#"{composer}{{position:relative;isolation:isolate;overflow:visible !important;background:rgba({surface_rgb},{opacity:.2}) !important;border:1px solid rgba(255,211,151,0.72) !important;border-radius:22px !important;box-shadow:inset 0 1px 0 rgba(255,255,255,0.20),0 18px 48px rgba(0,0,0,0.18),0 0 18px rgba(255,211,151,0.08)}}{message}{{position:relative;isolation:isolate;overflow:visible !important;background:rgba({surface_rgb},{opacity:.2}) !important;border:1px solid rgba(255,211,151,0.62) !important;border-radius:18px 18px 6px 18px !important;box-shadow:inset 0 1px 0 rgba(255,255,255,0.16),0 10px 28px rgba(0,0,0,0.14)}}"#,
        composer = COMPOSER_SELECTOR,
        message = USER_MESSAGE_SELECTOR,
    );
    format!("{surfaces}{}", conversation_decoration_css())
}
```

- [ ] **Step 5: 格式化并运行两个定向测试**

Run:

```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml --locked --test skin_adapter composer_and_user_messages_share_opacity_but_keep_independent_shapes -- --exact --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --locked --test skin_adapter botanical_baroque_decorations_use_fixed_noninteractive_svg_layers -- --exact --nocapture
```

Expected: 两项 PASS，最终 CSS 中恰好出现六个 `data:image/svg+xml`。

- [ ] **Step 6: 提交固定 SVG 装饰**

```powershell
git add -- src-tauri/src/skin/adapter.rs src-tauri/tests/skin_adapter.rs
git commit -m "feat: add botanical baroque chat borders"
```

---

### Task 3: 锁定窄屏降级、玻璃组合与安全回退

**Files:**
- Modify: `src-tauri/src/skin/adapter.rs:96-190`
- Test: `src-tauri/tests/skin_adapter.rs:296-365`

**Interfaces:**
- Consumes: Task 2 的 `conversation_decoration_css() -> String` 和四层输入卡片背景顺序 `左上藤蔓、中央卷草、右下藤蔓、底边点珠`。
- Produces: `@media(max-width:900px)` 降级规则；零毛玻璃、正毛玻璃和固定清理路径均继续保留装饰或完整回退。

- [ ] **Step 1: 添加窄屏和玻璃组合失败测试**

在 SVG 装饰测试后增加：

```rust
#[test]
fn narrow_windows_hide_secondary_composer_ornaments_without_removing_corners() {
    let css = execute_style_text(&fixture_settings());

    assert!(css.contains(
        "@media(max-width:900px){[data-composer-card]::before{background-size:72px 64px,0 0,72px 64px,0 0}}"
    ));
    assert_eq!(css.matches("data:image/svg+xml").count(), 6);
}

#[test]
fn fixed_conversation_ornaments_do_not_depend_on_glass_blur() {
    for radius in [0, 16] {
        let mut settings = fixture_settings();
        settings.glass_blur_px = radius;
        let css = execute_style_text(&settings);

        assert!(css.contains("[data-composer-card]::before"));
        assert!(css.contains(
            "[data-chat-flow-kind=\"user\"] [data-slot=\"conversation.message.images\"]+div::before"
        ));
        assert_eq!(css.matches("data:image/svg+xml").count(), 6);
    }
}
```

- [ ] **Step 2: 运行测试并确认缺少媒体查询**

Run:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --locked --test skin_adapter narrow_windows_hide_secondary_composer_ornaments_without_removing_corners -- --exact --nocapture
```

Expected: FAIL，缺少 `@media(max-width:900px)`。

- [ ] **Step 3: 把响应式规则追加到装饰 CSS**

在 `conversation_decoration_css` 的 `format!` 模板尾部、最终引号前追加：

```css
@media(max-width:900px){[data-composer-card]::before{background-size:72px 64px,0 0,72px 64px,0 0}}
```

在 Rust 原始字符串中保持单行并转义格式化花括号：

```rust
@media(max-width:900px){{{composer}::before{{background-size:72px 64px,0 0,72px 64px,0 0}}}}
```

该规则只把第二、第四层尺寸降为零；第一、第三层角花仍引用原固定 SVG，避免复制 data URI 或改变六个资源的计数。

- [ ] **Step 4: 更新零毛玻璃回归测试的语义**

把 `zero_glass_blur_keeps_one_wallpaper_without_glass_decorations` 重命名为 `zero_glass_blur_keeps_one_wallpaper_without_backdrop_filter`，保留原断言并增加：

```rust
assert!(css.contains("[data-composer-card]::before"));
assert_eq!(css.matches("data:image/svg+xml").count(), 6);
```

“无毛玻璃”只表示没有 `backdrop-filter`，不能再表示没有固定对话边框。

- [ ] **Step 5: 运行定向测试和完整适配器回归**

Run:

```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml --locked --test skin_adapter narrow_windows_hide_secondary_composer_ornaments_without_removing_corners -- --exact --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --locked --test skin_adapter fixed_conversation_ornaments_do_not_depend_on_glass_blur -- --exact --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --locked --test skin_adapter zero_glass_blur_keeps_one_wallpaper_without_backdrop_filter -- --exact --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --locked --test skin_adapter -- --nocapture
```

Expected: 全部 PASS；来源、版本、唯一壁纸、清理脚本和脚本注入防护断言保持通过。

- [ ] **Step 6: 提交响应式与回归保护**

```powershell
git add -- src-tauri/src/skin/adapter.rs src-tauri/tests/skin_adapter.rs
git commit -m "test: cover decorative skin fallbacks"
```

---

### Task 4: 完整自动验证与 Windows 实际验收

**Files:**
- Verify only: `src-tauri/src/skin/adapter.rs`
- Verify only: `src-tauri/tests/skin_adapter.rs`

**Interfaces:**
- Consumes: Tasks 1–3 提交后的固定 CSS 注入结果。
- Produces: 格式、静态分析、Rust/TypeScript 回归、构建和真实 WebView2 验收证据；不新增生产接口。

- [ ] **Step 1: 检查格式和工作树范围**

Run:

```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
git diff --check
git status --short
```

Expected: 格式和 diff 检查无错误；工作树只包含实施范围内的已知改动以及用户已有的 `.superpowers/` 未跟踪预览目录。

- [ ] **Step 2: 运行完整 Rust 测试**

Run:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --locked
```

Expected: PASS。

- [ ] **Step 3: 运行 Clippy 并拒绝警告**

Run:

```powershell
cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets -- -D warnings
```

Expected: PASS，无 warning。

- [ ] **Step 4: 运行前端测试和构建**

Run:

```powershell
pnpm test
pnpm build
```

Expected: 两条命令均 PASS。

- [ ] **Step 5: 在 Windows WebView2 执行固定验收矩阵**

启动当前开发构建：

```powershell
pnpm tauri dev
```

在打开的 Windows WebView2 主窗口逐项记录结果：

```text
1. 深色遮罩：输入卡片完整显示左上藤蔓、中央卷草、右下藤蔓和底边点珠。
2. 浅色遮罩：正文、占位文字和工具按钮保持可读，固定装饰色不吞没边界。
3. 用户消息“你好”：气泡只显示右上短藤蔓/花和左下短卷草。
4. 长用户消息：角花尺寸不拉伸，换行文字不与装饰重叠。
5. 多行输入：输入卡片高度变化时角花锚定边角，中央和底边图层不纵向拉伸。
6. 聚焦、选择、复制、附件入口、模式、工作区、模型和发送按钮均可操作。
7. 窗口宽度不大于 900px：中央卷草和点珠隐藏，两个角花保留。
8. 关闭沉浸式皮肤并重新导航：装饰完整消失，官方样式恢复。
```

Expected: 八项全部通过；任何失败都回到对应任务先补失败测试，再修复生产代码，不以手工验收替代自动测试。

- [ ] **Step 6: 确认最终提交范围**

Run:

```powershell
git log -3 --oneline
git status --short
```

Expected: 最近三个实施提交依次覆盖独立表面、固定 SVG 和回退测试；没有未提交的生产代码。`.superpowers/` 保持未跟踪且不删除、不提交。
