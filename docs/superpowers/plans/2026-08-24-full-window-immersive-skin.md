# Main Window Full-Window Immersive Skin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让主聊天窗口使用覆盖自绘标题栏、侧栏和内容区的连续皮肤图片，并使 DSH 对话输入卡片透明。

**Architecture:** 仅将 `main` 设为无系统装饰；Rust 桌面壳在本地启动页和受信 DSH 页面加载完成后注入同一套标题栏 DOM/CSS/事件脚本，并用一个封闭的原生命令执行四种窗口动作。现有皮肤适配器继续在 `body` 绘制唯一背景，只额外透明化侧栏主题变量和经过版本验证的 `[data-composer-card]` 表面。

**Tech Stack:** Rust 2024、Tauri 2.11、serde、JavaScript 注入脚本、Node 可执行 harness、Cargo test、Vitest、PowerShell 7。

**Spec:** `docs/superpowers/specs/2026-08-24-full-window-immersive-skin-design.md`

## Global Constraints

- 只改变 `main` 主聊天窗口；`updates` 与 `appearance` 继续使用 Windows 原生标题栏。
- 不新增生产依赖，不改变皮肤设置 schema、图片协议、runtime 更新链路或托盘退出语义。
- 只支持精确验证的 DSH `0.1.1-rc.2` DOM 合约；未知 runtime 继续失败关闭皮肤。
- 官方 DSH 页面只能获得 `control_main_window` 与既有 `report_skin_adapter` 两项窄命令能力。
- 公共函数、Tauri Command 和 Agent/Tool 层接口必须有类型注解；公共函数使用中文 Sphinx 风格 Docstring，包含 `:param:`、`:return:`、`:raises:`。
- 动态错误正文、URL、用户路径和皮肤设置不得进入页面或诊断日志。
- 全部实现遵循 Red-Green-Refactor；先运行能捕获用户症状的失败测试，再修改生产代码。
- Windows 命令优先使用 PowerShell 7；不得删除文件。

---

### Task 1: 建立受限且可测试的主窗口控制边界

**Files:**
- Create: `src-tauri/src/window_chrome.rs`
- Modify: `src-tauri/src/skin/adapter.rs`
- Modify: `src-tauri/src/lib.rs`
- Test: `src-tauri/tests/window_chrome.rs`

**Interfaces:**
- Consumes: `SkinAdapterController` 当前绑定的精确数字回环来源。
- Produces: `MainWindowAction`、`MainWindowControlState`、`control_main_window(window, adapter, action)`、`SkinAdapterController::allows_page(url)`。

- [ ] **Step 1: 写出窗口动作、来源和行为的失败测试**

在 `src-tauri/tests/window_chrome.rs` 创建真实的序列化与来源策略测试；每个断言分别捕获“未知动作被接受”“辅助窗口获权”“任意回环页获权”三类回归：

```rust
use dsh_desktop_lib::skin::SkinAdapterController;
use dsh_desktop_lib::window_chrome::{
    MainWindowAction, main_window_control_allowed,
};

#[test]
fn main_window_action_schema_accepts_only_the_four_reviewed_actions() {
    for (json, expected) in [
        (r#""start_dragging""#, MainWindowAction::StartDragging),
        (r#""minimize""#, MainWindowAction::Minimize),
        (r#""toggle_maximize""#, MainWindowAction::ToggleMaximize),
        (r#""close""#, MainWindowAction::Close),
    ] {
        let parsed: MainWindowAction = serde_json::from_str(json).expect("reviewed action");
        assert_eq!(parsed, expected);
    }
    assert!(serde_json::from_str::<MainWindowAction>(r#""maximize""#).is_err());
    assert!(serde_json::from_str::<MainWindowAction>(r#""exit""#).is_err());
}

#[test]
fn window_control_rejects_auxiliary_and_unbound_remote_pages() {
    let controller = SkinAdapterController::default();
    let unbound = tauri::Url::parse("http://127.0.0.1:43128/chat").unwrap();
    assert!(!main_window_control_allowed("updates", &unbound, &controller));
    assert!(!main_window_control_allowed("main", &unbound, &controller));
    assert!(!main_window_control_allowed(
        "main",
        &tauri::Url::parse("https://example.invalid/").unwrap(),
        &controller,
    ));
}

#[test]
fn bundled_main_origin_remains_controllable_before_runtime_navigation() {
    let controller = SkinAdapterController::default();
    for url in ["tauri://localhost/", "http://tauri.localhost/"] {
        assert!(main_window_control_allowed(
            "main",
            &tauri::Url::parse(url).unwrap(),
            &controller,
        ));
    }
}

#[cfg(debug_assertions)]
#[test]
fn vite_main_origin_remains_controllable_in_debug_builds() {
    assert!(main_window_control_allowed(
        "main",
        &tauri::Url::parse("http://127.0.0.1:1420/").unwrap(),
        &SkinAdapterController::default(),
    ));
}
```

- [ ] **Step 2: 运行测试并确认因接口缺失而失败**

Run:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test window_chrome --locked
```

Expected: 编译失败，报告 `dsh_desktop_lib::window_chrome` 不存在；不能是 fixture 或 Cargo 配置错误。

- [ ] **Step 3: 给皮肤适配器增加只读的当前来源校验**

在 `SkinAdapterController` 实现中加入以下方法；复用既有 `numeric_loopback_origin`，不重新放宽 URL 规则：

```rust
/// 判断页面是否仍匹配原生侧当前绑定的精确 DSH 来源。
///
/// :param url: 当前调用方 WebView 的完整 URL。
/// :return: 仅数字回环 scheme、host、port 与活动绑定全部一致时返回 `true`。
/// :raises: 状态锁异常时失败关闭为 `false`。
pub(crate) fn allows_page(&self, url: &tauri::Url) -> bool {
    let Ok(state) = self.state.lock() else {
        return false;
    };
    numeric_loopback_origin(url).is_some_and(|origin| {
        state
            .active
            .as_ref()
            .is_some_and(|active| active.origin == origin)
    })
}
```

- [ ] **Step 4: 实现封闭窗口动作和真实 Tauri 命令**

创建 `src-tauri/src/window_chrome.rs`。窗口操作细节收敛在私有 trait，使单元测试可以用内存窗口状态执行真实分支；Tauri 命令只负责取得 URL、再次授权并转发：

```rust
use crate::skin::SkinAdapterController;
use serde::{Deserialize, Serialize};
use tauri::{State, WebviewWindow};

const CONTROL_DENIED: &str = "main_window_control_denied";
const CONTROL_FAILED: &str = "main_window_control_failed";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MainWindowAction {
    StartDragging,
    Minimize,
    ToggleMaximize,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MainWindowControlState {
    pub maximized: bool,
}

trait WindowControl {
    fn start_dragging(&self) -> tauri::Result<()>;
    fn minimize(&self) -> tauri::Result<()>;
    fn is_maximized(&self) -> tauri::Result<bool>;
    fn maximize(&self) -> tauri::Result<()>;
    fn unmaximize(&self) -> tauri::Result<()>;
    fn close(&self) -> tauri::Result<()>;
}

impl WindowControl for WebviewWindow {
    fn start_dragging(&self) -> tauri::Result<()> { WebviewWindow::start_dragging(self) }
    fn minimize(&self) -> tauri::Result<()> { WebviewWindow::minimize(self) }
    fn is_maximized(&self) -> tauri::Result<bool> { WebviewWindow::is_maximized(self) }
    fn maximize(&self) -> tauri::Result<()> { WebviewWindow::maximize(self) }
    fn unmaximize(&self) -> tauri::Result<()> { WebviewWindow::unmaximize(self) }
    fn close(&self) -> tauri::Result<()> { WebviewWindow::close(self) }
}

fn execute_window_action(
    window: &impl WindowControl,
    action: MainWindowAction,
) -> Result<MainWindowControlState, &'static str> {
    match action {
        MainWindowAction::StartDragging => window.start_dragging(),
        MainWindowAction::Minimize => window.minimize(),
        MainWindowAction::ToggleMaximize => {
            if window.is_maximized().map_err(|_| CONTROL_FAILED)? {
                window.unmaximize()
            } else {
                window.maximize()
            }
        }
        MainWindowAction::Close => window.close(),
    }
    .map_err(|_| CONTROL_FAILED)?;
    Ok(MainWindowControlState {
        maximized: window.is_maximized().unwrap_or(false),
    })
}

fn is_bundled_main_url(url: &tauri::Url) -> bool {
    let bundled = matches!(
        (url.scheme(), url.host_str()),
        ("tauri", Some("localhost")) | ("http", Some("tauri.localhost"))
    );
    #[cfg(debug_assertions)]
    let development = url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.port() == Some(1420);
    #[cfg(not(debug_assertions))]
    let development = false;
    bundled || development
}

pub fn main_window_control_allowed(
    label: &str,
    url: &tauri::Url,
    adapter: &SkinAdapterController,
) -> bool {
    label == "main" && (is_bundled_main_url(url) || adapter.allows_page(url))
}

/// 执行主窗口自绘标题栏允许的一个封闭动作。
///
/// :param window: Tauri 注入的当前调用方 WebView 窗口。
/// :param adapter: 原生侧已绑定精确 DSH 来源的适配器状态。
/// :param action: 四种经过审核的窗口动作之一。
/// :return: 动作后的最大化状态；关闭动作可能在响应前隐藏窗口。
/// :raises str: 标签、来源或底层窗口调用失败时返回固定错误类别。
#[tauri::command]
pub fn control_main_window(
    window: WebviewWindow,
    adapter: State<'_, SkinAdapterController>,
    action: MainWindowAction,
) -> Result<MainWindowControlState, &'static str> {
    let url = window.url().map_err(|_| CONTROL_DENIED)?;
    if !main_window_control_allowed(window.label(), &url, adapter.inner()) {
        return Err(CONTROL_DENIED);
    }
    execute_window_action(&window, action)
}
```

给 `main_window_control_allowed` 补充 Sphinx 风格中文 Docstring，逐项说明 `label`、`url`、`adapter`、布尔返回值和锁异常失败关闭。

在该模块的 `#[cfg(test)]` 中加入以下内存窗口。测试断言动作后的真实状态，不断言 mock 调用次数：

```rust
#[cfg(test)]
mod tests {
    use super::{
        MainWindowAction, WindowControl, execute_window_action,
        main_window_control_allowed,
    };
    use crate::runtime::install_state::RuntimeSkinCompatibility;
    use crate::skin::SkinAdapterController;
    use semver::Version;
    use std::cell::Cell;

    #[derive(Default)]
    struct FakeWindow {
        dragging: Cell<bool>,
        minimized: Cell<bool>,
        maximized: Cell<bool>,
        closed: Cell<bool>,
    }

    impl WindowControl for FakeWindow {
        fn start_dragging(&self) -> tauri::Result<()> { self.dragging.set(true); Ok(()) }
        fn minimize(&self) -> tauri::Result<()> { self.minimized.set(true); Ok(()) }
        fn is_maximized(&self) -> tauri::Result<bool> { Ok(self.maximized.get()) }
        fn maximize(&self) -> tauri::Result<()> { self.maximized.set(true); Ok(()) }
        fn unmaximize(&self) -> tauri::Result<()> { self.maximized.set(false); Ok(()) }
        fn close(&self) -> tauri::Result<()> { self.closed.set(true); Ok(()) }
    }

    #[test]
    fn toggle_maximize_changes_real_window_state_in_both_directions() {
        let window = FakeWindow::default();
        let maximized = execute_window_action(&window, MainWindowAction::ToggleMaximize).unwrap();
        assert!(window.maximized.get());
        assert!(maximized.maximized);
        let restored = execute_window_action(&window, MainWindowAction::ToggleMaximize).unwrap();
        assert!(!window.maximized.get());
        assert!(!restored.maximized);
    }

    #[test]
    fn close_uses_the_window_close_path_without_process_exit() {
        let window = FakeWindow::default();
        execute_window_action(&window, MainWindowAction::Close).unwrap();
        assert!(window.closed.get());
    }

    #[test]
    fn remote_control_requires_the_current_bound_numeric_origin() {
        let controller = SkinAdapterController::default();
        let official = tauri::Url::parse("http://127.0.0.1:43127/chat").unwrap();
        let other = tauri::Url::parse("http://127.0.0.1:43128/chat").unwrap();
        assert!(controller.bind_navigation(
            &Version::parse("0.1.1-rc.2").unwrap(),
            RuntimeSkinCompatibility::Verified,
            &official,
        ));
        assert!(main_window_control_allowed("main", &official, &controller));
        assert!(!main_window_control_allowed("main", &other, &controller));
    }
}
```

- [ ] **Step 5: 暴露模块并注册命令**

在 `src-tauri/src/lib.rs` 增加：

```rust
pub mod window_chrome;
use window_chrome::control_main_window;
```

并把 `control_main_window` 加入 `tauri::generate_handler!`。此时先不改 capability，保证命令即使被注册也尚未授予页面。

- [ ] **Step 6: 运行定向测试并确认通过**

Run:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test window_chrome --locked
cargo test --manifest-path src-tauri/Cargo.toml window_chrome --locked
```

Expected: 两条命令均退出码 0；来源策略与内存窗口分支全部通过。

- [ ] **Step 7: 提交窗口控制边界**

```powershell
git add -- src-tauri/src/window_chrome.rs src-tauri/src/skin/adapter.rs src-tauri/src/lib.rs src-tauri/tests/window_chrome.rs
git commit -m "feat: add bounded main window controls"
```

---

### Task 2: 注入单一自绘标题栏并只让主窗口无边框

**Files:**
- Modify: `src-tauri/src/window_chrome.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/diagnostics.rs`
- Modify: `src-tauri/tauri.conf.json`
- Modify: `src-tauri/build.rs`
- Create: `src-tauri/capabilities/main-window-chrome-local.json`
- Create: `src-tauri/capabilities/main-window-chrome-remote.json`
- Create (generated by Tauri build): `src-tauri/permissions/autogenerated/control_main_window.toml`
- Modify: `src-tauri/tests/window_chrome.rs`
- Modify: `src-tauri/tests/command_permissions.rs`

**Interfaces:**
- Consumes: Task 1 的 `control_main_window` 命令和 `MainWindowControlState.maximized` 响应。
- Produces: `titlebar_script() -> &'static str`、`sync_maximized_script(maximized: bool) -> String`、`apply_to_main(webview: &Webview) -> Result<(), ()>`。

- [ ] **Step 1: 写出标题栏可执行行为与窗口配置的失败测试**

在 `src-tauri/tests/window_chrome.rs` 增加 Node harness。测试执行真实 `titlebar_script()` 两次，模拟 DOM 和 `__TAURI_INTERNALS__.invoke`，并用页面最终状态断言单例标题栏、正确动作映射和最大化可访问状态：

```rust
use dsh_desktop_lib::window_chrome::{sync_maximized_script, titlebar_script};
use std::process::Command;

#[test]
fn executable_titlebar_is_singleton_and_maps_real_user_events() {
    let harness = format!(
        r#"
const actions=[];
class Node {{
  constructor(tag) {{ this.tag=tag; this.id=''; this.dataset={{}}; this.children=[]; this.listeners={{}}; this.attributes={{}}; this.parent=null; }}
  append(...children) {{ for(const child of children){{child.parent=this;this.children.push(child)}} }}
  addEventListener(name, handler) {{ this.listeners[name]=handler; }}
  remove() {{ if(this.parent)this.parent.children=this.parent.children.filter((child)=>child!==this);if(this.id)nodes.delete(this.id); }}
  setAttribute(name, value) {{ this.attributes[name]=String(value); }}
  closest(selector) {{ return selector.startsWith('button') && this.tag==='button' ? this : null; }}
  contains(candidate) {{ return candidate===this || this.children.some((child)=>child.contains?.(candidate)); }}
  querySelector(selector) {{
    const action=selector.match(/^\[data-action=([^\]]+)\]$/)?.[1];
    if(action&&this.dataset.action===action)return this;
    for(const child of this.children){{const found=child.querySelector?.(selector);if(found)return found}}
    return null;
  }}
}}
const nodes=new Map();
const body=new Node('body');
body.prepend=(node)=>{{node.parent=body;nodes.set(node.id,node);body.children.unshift(node);}};
const head=new Node('head');
head.append=(node)=>{{node.parent=head;nodes.set(node.id,node);head.children.push(node);}};
global.document={{
  body, head,
  getElementById:(id)=>nodes.get(id)??null,
  createElement:(tag)=>new Node(tag),
  createElementNS:(_namespace,tag)=>new Node(tag),
}};
global.__TAURI_INTERNALS__={{invoke:(_name,args)=>{{actions.push(args.action);return Promise.resolve({{maximized:args.action==='toggle_maximize'}});}}}};
{script}
{script}
const bar=nodes.get('dsh-desktop-titlebar');
const drag={{button:0,target:bar,detail:1}};
bar.listeners.pointerdown(drag);
bar.listeners.dblclick({{button:0,target:bar}});
const close=bar.querySelector('[data-action=close]');
bar.listeners.click({{target:close}});
setImmediate(()=>{{
  {sync}
  const ok=nodes.size===2
    && body.children.filter((node)=>node.id==='dsh-desktop-titlebar').length===1
    && actions.join(',')==='start_dragging,toggle_maximize,close'
    && bar.attributes['data-maximized']==='true';
  console.log(ok?'TITLEBAR_OK':'TITLEBAR_BAD');
}});
"#,
        script = titlebar_script(),
        sync = sync_maximized_script(true),
    );
    let output = Command::new("node").args(["-e", &harness]).output().expect("node");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "TITLEBAR_OK");
}
```

在 `src-tauri/tests/command_permissions.rs` 增加 JSON 行为约束：

```rust
#[test]
fn only_main_is_frameless_and_window_control_has_two_narrow_capabilities() {
    let config: Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
    let windows = config["app"]["windows"].as_array().unwrap();
    let main = windows.iter().find(|item| item["label"] == "main").unwrap();
    let updates = windows.iter().find(|item| item["label"] == "updates").unwrap();
    let appearance = windows.iter().find(|item| item["label"] == "appearance").unwrap();
    assert_eq!(main["decorations"], false);
    assert!(updates.get("decorations").is_none());
    assert!(appearance.get("decorations").is_none());

    let local: Value = serde_json::from_str(include_str!(
        "../capabilities/main-window-chrome-local.json"
    )).unwrap();
    let remote: Value = serde_json::from_str(include_str!(
        "../capabilities/main-window-chrome-remote.json"
    )).unwrap();
    assert_eq!(local["local"], true);
    assert_eq!(local["windows"], serde_json::json!(["main"]));
    assert_eq!(remote["local"], false);
    assert_eq!(remote["windows"], serde_json::json!(["main"]));
    assert_eq!(remote["remote"]["urls"], serde_json::json!(["http://127.0.0.1:*"]));
    assert_eq!(local["permissions"], serde_json::json!(["allow-control-main-window"]));
    assert_eq!(remote["permissions"], serde_json::json!(["allow-control-main-window"]));
}
```

- [ ] **Step 2: 运行两项测试并确认分别因脚本和配置缺失而失败**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test window_chrome executable_titlebar_is_singleton_and_maps_real_user_events --locked -- --exact --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --test command_permissions only_main_is_frameless_and_window_control_has_two_narrow_capabilities --locked -- --exact --nocapture
```

Expected: 第一项编译失败，报告脚本接口缺失；第二项因 capability 文件或 `main.decorations` 缺失失败。

- [ ] **Step 3: 实现固定标题栏脚本**

在 `window_chrome.rs` 增加 `TITLEBAR_ID`、`TITLEBAR_STYLE_ID` 和静态脚本。脚本必须采用 DOM API 和 `textContent`/静态 SVG，不使用页面提供的数据或 `innerHTML` 拼接动态值。核心结构如下，实际实现保持这些 ID、动作和可访问属性一致：

```rust
pub const TITLEBAR_ID: &str = "dsh-desktop-titlebar";
const TITLEBAR_STYLE_ID: &str = "dsh-desktop-titlebar-style";

/// 返回不读取页面业务数据的固定自绘标题栏脚本。
///
/// :return: 可重复执行且只保留一个标题栏的静态 JavaScript。
/// :raises: 静态字符串读取不产生错误。
pub fn titlebar_script() -> &'static str {
    r#"(()=>{'use strict';
const B='dsh-desktop-titlebar',S='dsh-desktop-titlebar-style',C='control_main_window';
document.getElementById(B)?.remove();document.getElementById(S)?.remove();
const style=document.createElement('style');style.id=S;
style.textContent=':root{--dsh-desktop-titlebar-height:31px}body{box-sizing:border-box!important;height:100vh!important;padding-top:var(--dsh-desktop-titlebar-height)!important}#app,#root{height:100%!important;min-height:0!important}#dsh-desktop-titlebar{position:fixed;z-index:2147483647;top:0;right:0;left:0;height:31px;display:flex;align-items:center;color:var(--dsw-alias-label-secondary,#c9c9cc);font:12px/1 "Segoe UI",sans-serif;user-select:none;-webkit-user-select:none}#dsh-desktop-titlebar [data-title]{display:flex;align-items:center;gap:7px;padding-left:8px;pointer-events:none}#dsh-desktop-titlebar svg{width:15px;height:15px;color:#67c8ff}#dsh-desktop-titlebar [data-drag]{flex:1;align-self:stretch}#dsh-desktop-titlebar button{width:46px;height:31px;border:0;color:inherit;background:transparent;display:grid;place-items:center}#dsh-desktop-titlebar button:hover,#dsh-desktop-titlebar button:focus-visible{background:rgb(255 255 255 / 12%);outline:none}#dsh-desktop-titlebar button[data-action=close]:hover{color:#fff;background:#c42b1c}#dsh-desktop-titlebar [data-restore]{display:none}#dsh-desktop-titlebar[data-maximized=true] [data-maximize]{display:none}#dsh-desktop-titlebar[data-maximized=true] [data-restore]{display:block}';
const bar=document.createElement('header');bar.id=B;bar.setAttribute('role','banner');bar.setAttribute('aria-label','DSH Desktop 标题栏');bar.setAttribute('data-maximized','false');
const title=document.createElement('span');title.setAttribute('data-title','');
const mark=document.createElementNS('http://www.w3.org/2000/svg','svg');mark.setAttribute('viewBox','0 0 24 24');mark.setAttribute('aria-hidden','true');
const markPath=document.createElementNS('http://www.w3.org/2000/svg','path');markPath.setAttribute('fill','currentColor');markPath.setAttribute('d','M3 13c2.5 0 3.8-1.2 5-3.7.8 1.4 2 2.3 3.7 2.7 2.1.5 4.2.1 6.3-1.2-.4 4.9-3.2 7.2-8.1 7.2C6 18 3.6 16.3 3 13Zm14.2-4.6c1.5-.1 2.7-.8 3.8-2.1.2 2.3-.6 4-2.5 5.1l-1.3-3Z');mark.append(markPath);
const label=document.createElement('span');label.textContent='DSH Desktop';title.append(mark,label);
const drag=document.createElement('span');drag.setAttribute('data-drag','');
const controls=document.createElement('nav');controls.setAttribute('aria-label','窗口控制');
const minimize=document.createElement('button');minimize.type='button';minimize.dataset.action='minimize';minimize.setAttribute('aria-label','最小化');minimize.textContent='—';
const toggle=document.createElement('button');toggle.type='button';toggle.dataset.action='toggle_maximize';toggle.setAttribute('aria-label','最大化');
const maximizeGlyph=document.createElement('span');maximizeGlyph.setAttribute('data-maximize','');maximizeGlyph.textContent='□';
const restoreGlyph=document.createElement('span');restoreGlyph.setAttribute('data-restore','');restoreGlyph.textContent='❐';toggle.append(maximizeGlyph,restoreGlyph);
const close=document.createElement('button');close.type='button';close.dataset.action='close';close.setAttribute('aria-label','关闭');close.textContent='×';controls.append(minimize,toggle,close);
bar.append(title,drag,controls);document.head.append(style);document.body.prepend(bar);
const invoke=(action)=>globalThis.__TAURI_INTERNALS__?.invoke(C,{action})?.then((state)=>{if(state&&typeof state.maximized==='boolean'){bar.setAttribute('data-maximized',String(state.maximized));const toggle=bar.querySelector?.('[data-action=toggle_maximize]');toggle?.setAttribute('aria-label',state.maximized?'还原':'最大化')}})?.catch(()=>{});
bar.addEventListener('pointerdown',(event)=>{if(event.button===0&&!event.target.closest('button'))void invoke('start_dragging')});
bar.addEventListener('dblclick',(event)=>{if(event.button===0&&!event.target.closest('button'))void invoke('toggle_maximize')});
bar.addEventListener('click',(event)=>{const button=event.target.closest('button[data-action]');if(button&&bar.contains(button))void invoke(button.dataset.action)});
})()"#
}
```

- [ ] **Step 4: 实现最大化状态同步和标题栏注入边界**

```rust
/// 生成只更新桌面壳标题栏最大化状态的固定值脚本。
pub fn sync_maximized_script(maximized: bool) -> String {
    format!(
        "(()=>{{const bar=document.getElementById('{TITLEBAR_ID}');if(!bar)return;bar.setAttribute('data-maximized','{maximized}');const button=bar.querySelector('[data-action=toggle_maximize]');button?.setAttribute('aria-label','{}')}})();",
        if maximized { "还原" } else { "最大化" }
    )
}

/// 在主 WebView 当前页面注入自绘标题栏。
pub(crate) fn apply_to_main(webview: &tauri::Webview) -> Result<(), ()> {
    if webview.label() != "main" {
        return Ok(());
    }
    webview.eval(titlebar_script()).map_err(|_| ())?;
    let maximized = webview.window().is_maximized().map_err(|_| ())?;
    webview
        .eval(sync_maximized_script(maximized))
        .map_err(|_| ())
}
```

- [ ] **Step 5: 接入页面加载和窗口 resize 生命周期**

重排 `lib.rs` 的 `on_page_load`：Started 分支只在适配器状态存在时使皮肤令牌失效；Finished 分支先注入标题栏，之后才获取皮肤适配器和设置。这样标题栏不依赖皮肤状态是否成功初始化：

```rust
let app = webview.app_handle();
if payload.event() == tauri::webview::PageLoadEvent::Started {
    record_skin_stage(app, DiagnosticStage::SkinPageStarted, None);
    if let Some(adapter) = app.try_state::<SkinAdapterController>() {
        adapter.navigation_started(payload.url());
    }
    return;
}
record_skin_stage(app, DiagnosticStage::SkinPageFinished, None);
if window_chrome::apply_to_main(webview).is_err() {
    record_app_diagnostic(
        app,
        DiagnosticStage::WindowChromeApply,
        DiagnosticErrorKind::TauriError,
    );
}
let Some(adapter) = app.try_state::<SkinAdapterController>() else {
    return;
};
let Some(skins) = app.try_state::<SkinController>() else {
    return;
};
if skin::adapter::apply_to_main(webview, payload.url(), &adapter, &skins).is_err() {
    record_app_diagnostic(app, DiagnosticStage::SkinApply, DiagnosticErrorKind::TauriError);
}
```

在 `on_window_event` 的 CloseRequested 匹配前处理主窗口 resize：

```rust
if window.label() == "main"
    && matches!(event, tauri::WindowEvent::Resized(_))
    && let Some(main) = window.app_handle().get_webview_window("main")
    && let Ok(maximized) = main.is_maximized()
    && main.eval(window_chrome::sync_maximized_script(maximized)).is_err()
{
    record_app_diagnostic(
        window.app_handle(),
        DiagnosticStage::WindowChromeApply,
        DiagnosticErrorKind::TauriError,
    );
}
```

保留后续现有 `CloseRequested` 分支原样，使 `control_main_window(Close)` 仍触发现有隐藏到托盘决策。

- [ ] **Step 6: 增加固定诊断阶段并更新持久化白名单**

在 `DiagnosticStage` 添加 `WindowChromeAction` 和 `WindowChromeApply`，并在 `is_persisted_stage` 中加入字符串 `window_chrome_action`、`window_chrome_apply`。在 `lib.rs` 增加固定诊断包装器：

```rust
pub(crate) fn record_window_chrome_action_diagnostic(app: &AppHandle) {
    record_app_diagnostic(
        app,
        DiagnosticStage::WindowChromeAction,
        DiagnosticErrorKind::TauriError,
    );
}
```

同时把 Task 1 命令尾部替换为以下实现，并在 `window_chrome.rs` 导入 `tauri::Manager`：

```rust
let result = execute_window_action(&window, action);
if result.is_err() {
    crate::record_window_chrome_action_diagnostic(window.app_handle());
}
result
```

窗口动作执行失败时调用包装器并继续向页面只返回 `main_window_control_failed`；来源拒绝不写日志，避免不可信页面用拒绝请求制造诊断噪声。不得记录底层 source。

- [ ] **Step 7: 配置主窗口无边框和双 capability**

在 `tauri.conf.json` 的 `main` 窗口加入：

```json
"decorations": false
```

并把两个 capability identifier 加入 `app.security.capabilities`：

```json
[
  "local-main",
  "local-appearance",
  "official-skin-report",
  "main-window-chrome-local",
  "main-window-chrome-remote"
]
```

创建本地 capability：

```json
{
  "$schema": "../gen/schemas/desktop-schema.json",
  "identifier": "main-window-chrome-local",
  "description": "本地主启动页仅可控制自身主窗口",
  "local": true,
  "windows": ["main"],
  "permissions": ["allow-control-main-window"]
}
```

创建远程 capability：

```json
{
  "$schema": "../gen/schemas/desktop-schema.json",
  "identifier": "main-window-chrome-remote",
  "description": "受信数字回环 DSH 页面仅可控制自身主窗口",
  "local": false,
  "remote": { "urls": ["http://127.0.0.1:*"] },
  "windows": ["main"],
  "permissions": ["allow-control-main-window"]
}
```

把 `control_main_window` 加入 `build.rs` 的 `COMMANDS`。运行一次定向 Cargo 测试让 Tauri 生成 `permissions/autogenerated/control_main_window.toml`，确认它只包含 allow/deny 该命令的两条权限。

- [ ] **Step 8: 运行定向测试并确认标题栏、权限和诊断全部通过**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test window_chrome --locked -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --test command_permissions --locked
cargo test --manifest-path src-tauri/Cargo.toml diagnostics --locked
```

Expected: 三条命令退出码均为 0；Node harness 输出 `TITLEBAR_OK`，辅助窗口装饰断言保持通过，诊断 JSONL 接受 `window_chrome_action` 与 `window_chrome_apply`。

- [ ] **Step 9: 提交可操作的无边框主窗口**

```powershell
git add -- src-tauri/src/window_chrome.rs src-tauri/src/lib.rs src-tauri/src/diagnostics.rs src-tauri/tauri.conf.json src-tauri/build.rs src-tauri/capabilities/main-window-chrome-local.json src-tauri/capabilities/main-window-chrome-remote.json src-tauri/permissions/autogenerated/control_main_window.toml src-tauri/tests/window_chrome.rs src-tauri/tests/command_permissions.rs
git commit -m "feat: render a bounded main window titlebar"
```

---

### Task 3: 让侧栏和对话输入卡片透出同一皮肤背景

**Files:**
- Modify: `src-tauri/src/skin/adapter.rs`
- Modify: `src-tauri/tests/skin_adapter.rs`

**Interfaces:**
- Consumes: 现有 `adapter_script(settings) -> Option<String>` 和 DSH `0.1.1-rc.2` 的 `--dsw-specific-sidebar-fill`、`[data-composer-card]` 合约。
- Produces: 全视口背景之上的透明侧栏与透明输入卡片；既有清理脚本仍只清理皮肤节点。

- [ ] **Step 1: 写出精确用户症状的失败测试**

在 `script_checks_dom_before_painting_the_page_canvas` 中增加以下行为约束；这些断言捕获“侧栏仍实色”“输入卡片仍实色”“误把所有 input-major 表面透明化”三种回归：

```rust
assert!(script.contains("--dsw-specific-sidebar-fill:transparent !important"));
assert!(script.contains("[data-composer-card]{background:transparent !important}"));
assert!(!script.contains("--dsw-specific-input-major:transparent"));
assert!(script.contains("--dsw-alias-bg-layer-1:rgba(255,255,255,0.88) !important"));
assert!(script.contains("--dsw-alias-bg-layer-2:rgba(255,255,255,0.88) !important"));
```

在 `cleanup_script_removes_existing_skin_nodes` 的 Node harness 中预置 `dsh-desktop-titlebar`，执行清理后断言标题栏未被访问或移除：

```rust
assert!(!cleanup_script().contains("dsh-desktop-titlebar"));
```

- [ ] **Step 2: 运行定向测试并确认因透明规则缺失而失败**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test skin_adapter script_checks_dom_before_painting_the_page_canvas --locked -- --exact --nocapture
```

Expected: FAIL，缺少 `--dsw-specific-sidebar-fill` 或 `[data-composer-card]`，而不是脚本生成返回 `None`。

- [ ] **Step 3: 最小修改适配器 CSS**

在 `adapter_script_for_page` 生成的 `style.textContent` 中，把主题覆盖调整为：

```css
:root,#root{
  --dsw-alias-bg-base:transparent !important;
  --dsw-alias-bg-layer-1:rgba({surface_rgb},{panel_opacity:.2}) !important;
  --dsw-alias-bg-layer-2:rgba({surface_rgb},{panel_opacity:.2}) !important;
  --dsw-specific-sidebar-fill:transparent !important;
  --dsh-desktop-border-opacity:{border_opacity:.2} !important;
}
[data-composer-card]{background:transparent !important}
```

保留现有 `body`/`body::before` 唯一背景、图片 URL、遮罩、模糊、`requestAnimationFrame` 有限重试和 DOM 兼容报告逻辑。不要覆盖 `--dsw-specific-input-major`，不要增加通用 `textarea` 或 CSS module hash 选择器。

- [ ] **Step 4: 运行皮肤适配器完整测试并确认通过**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test skin_adapter --locked -- --nocapture
```

Expected: 全部通过；可执行脚本仍输出 `RETRIED` 和 `FAIL_CLOSED`，清理测试确认标题栏不属于皮肤清理范围。

- [ ] **Step 5: 提交透明侧栏和输入卡片**

```powershell
git add -- src-tauri/src/skin/adapter.rs src-tauri/tests/skin_adapter.rs
git commit -m "fix: extend immersive skins across DSH chrome"
```

---

### Task 4: 更新用户说明并完成全量自动验证

**Files:**
- Modify: `docs/用户使用指南.md`
- Modify: `docs/development.md`

**Interfaces:**
- Consumes: Tasks 1–3 的最终行为。
- Produces: 用户可理解的主窗口范围说明和开发者可重复的 Windows 验收清单。

- [ ] **Step 1: 更新用户指南的皮肤效果说明**

在皮肤设置章节明确写入以下内容，不宣称辅助窗口也使用皮肤：

```markdown
启用沉浸式皮肤后，所选图片会连续覆盖主聊天窗口的自绘标题栏、左侧栏和对话区；对话输入卡片会透出背景。外观设置和更新窗口仍使用 Windows 原生标题栏。弹窗、菜单和确认界面会保留按“面板透明度”计算的底色，以保证内容可读。
```

- [ ] **Step 2: 更新开发文档的人工验收矩阵**

在 `docs/development.md` 的皮肤验证段落加入：

```markdown
- 主窗口标题栏：拖动、双击最大化、最小化、最大化/还原、关闭到托盘；
- 连续背景：标题栏、侧栏、会话区没有独立实色断层；
- 输入卡片：首页 hero 与普通会话 composer 均透明，文字和按钮可读；
- 回退：关闭皮肤或触发不兼容门禁后标题栏仍可操作；
- DPI：Windows 100% 与 125% 缩放下分别检查普通态和最大化态；
- 辅助窗口：外观设置和更新窗口继续显示原生标题栏。
```

- [ ] **Step 3: 运行格式、Rust、前端和安全 fixture 全量验证**

```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo test --manifest-path src-tauri/Cargo.toml --locked
cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets -- -D warnings
pnpm test
pnpm build
pwsh -NoProfile -File scripts/smoke-runtime.ps1 -SecurityFixturesOnly
```

Expected: 每条命令退出码 0；Cargo/Vitest 0 failures，Clippy 0 warnings，Vite build 成功，安全 fixture smoke 成功。

- [ ] **Step 4: 构建 release 安装包**

```powershell
pnpm tauri build
```

Expected: 退出码 0，并生成当前配置的 Windows NSIS 安装包。不要覆盖或删除用户现有安装数据。

- [ ] **Step 5: 安装或运行 release 后完成人工验收**

按以下固定顺序记录结果和截图：

```text
1. 100% 缩放：启动页标题栏可拖动，三个按钮可用。
2. 100% 缩放：DSH hero 页图片贯穿标题栏/侧栏/内容区，输入卡片透明。
3. 100% 缩放：普通会话底部输入卡片透明，弹窗和菜单仍可读。
4. 100% 缩放：双击、最大化/还原、最小化、关闭到托盘均符合现有语义。
5. 125% 缩放：重复普通态、最大化态、窗口边缘和内容遮挡检查。
6. 关闭皮肤：主题回退完整，标题栏仍可拖动和关闭。
7. 打开外观设置和更新窗口：两者仍显示原生标题栏。
```

若任一项失败，记录固定步骤和截图，回到对应 Task 的测试先构造 red-capable 复现，再修改生产代码。

- [ ] **Step 6: 提交文档更新**

只在 Step 3 全绿且 Step 5 全部通过后执行：

```powershell
git add -- docs/用户使用指南.md docs/development.md
git commit -m "docs: explain full-window immersive skins"
```
