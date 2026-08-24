use dsh_desktop_lib::skin::SkinAdapterController;
use dsh_desktop_lib::window_chrome::{
    MainWindowAction, main_window_control_allowed, sync_maximized_script, titlebar_script,
};
use std::process::Command;

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
    let unbound = tauri::Url::parse("http://127.0.0.1:43128/chat").expect("unbound url");

    assert!(!main_window_control_allowed(
        "updates",
        &unbound,
        &controller
    ));
    assert!(!main_window_control_allowed("main", &unbound, &controller));
    assert!(!main_window_control_allowed(
        "main",
        &tauri::Url::parse("https://example.invalid/").expect("remote url"),
        &controller,
    ));
}

#[test]
fn bundled_main_origin_remains_controllable_before_runtime_navigation() {
    let controller = SkinAdapterController::default();
    for url in ["tauri://localhost/", "http://tauri.localhost/"] {
        assert!(main_window_control_allowed(
            "main",
            &tauri::Url::parse(url).expect("bundled url"),
            &controller,
        ));
    }
}

#[cfg(debug_assertions)]
#[test]
fn vite_main_origin_remains_controllable_in_debug_builds() {
    assert!(main_window_control_allowed(
        "main",
        &tauri::Url::parse("http://127.0.0.1:1420/").expect("vite url"),
        &SkinAdapterController::default(),
    ));
}

#[test]
fn executable_titlebar_is_singleton_and_maps_real_user_events() {
    let harness = format!(
        r#"
const actions=[];
const nodes=new Map();
class Node {{
  constructor(tag) {{ this.tag=tag; this.id=''; this.dataset={{}}; this.children=[]; this.listeners={{}}; this.attributes={{}}; this.parent=null; this.textContent=''; }}
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
bar.listeners.pointerdown({{button:0,target:bar}});
bar.listeners.dblclick({{button:0,target:bar}});
const close=bar.querySelector('[data-action=close]');
bar.listeners.click({{target:close}});
setImmediate(()=>{{
  {sync}
  const toggle=bar.querySelector('[data-action=toggle_maximize]');
  const ok=nodes.size===2
    && body.children.filter((node)=>node.id==='dsh-desktop-titlebar').length===1
    && actions.join(',')==='start_dragging,toggle_maximize,close'
    && bar.attributes['data-maximized']==='true'
    && toggle.attributes['aria-label']==='还原';
  console.log(ok?'TITLEBAR_OK':'TITLEBAR_BAD');
}});
"#,
        script = titlebar_script(),
        sync = sync_maximized_script(true),
    );
    let output = Command::new("node")
        .args(["-e", &harness])
        .output()
        .expect("前端工具链必须提供 node");

    assert!(
        output.status.success(),
        "Node harness 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "TITLEBAR_OK"
    );
}
