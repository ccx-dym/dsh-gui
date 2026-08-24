use crate::skin::SkinAdapterController;
use serde::{Deserialize, Serialize};
use tauri::{Manager, State, Webview, WebviewWindow};

const CONTROL_DENIED: &str = "main_window_control_denied";
const CONTROL_FAILED: &str = "main_window_control_failed";
pub const TITLEBAR_ID: &str = "dsh-desktop-titlebar";

/// 自绘标题栏允许触发的封闭窗口动作。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MainWindowAction {
    StartDragging,
    Minimize,
    ToggleMaximize,
    Close,
}

/// 窗口动作完成后返回给标题栏的最小状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MainWindowControlState {
    pub maximized: bool,
}

trait WindowControl {
    fn start_dragging(&self) -> Result<(), ()>;
    fn minimize(&self) -> Result<(), ()>;
    fn is_maximized(&self) -> Result<bool, ()>;
    fn maximize(&self) -> Result<(), ()>;
    fn unmaximize(&self) -> Result<(), ()>;
    fn close(&self) -> Result<(), ()>;
}

impl WindowControl for WebviewWindow {
    fn start_dragging(&self) -> Result<(), ()> {
        WebviewWindow::start_dragging(self).map_err(|_| ())
    }

    fn minimize(&self) -> Result<(), ()> {
        WebviewWindow::minimize(self).map_err(|_| ())
    }

    fn is_maximized(&self) -> Result<bool, ()> {
        WebviewWindow::is_maximized(self).map_err(|_| ())
    }

    fn maximize(&self) -> Result<(), ()> {
        WebviewWindow::maximize(self).map_err(|_| ())
    }

    fn unmaximize(&self) -> Result<(), ()> {
        WebviewWindow::unmaximize(self).map_err(|_| ())
    }

    fn close(&self) -> Result<(), ()> {
        WebviewWindow::close(self).map_err(|_| ())
    }
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
        // 关闭动作可能已让平台句柄不可查询；响应只用于标题栏图标，失败时安全回退。
        maximized: window.is_maximized().unwrap_or(false),
    })
}

fn is_bundled_main_url(url: &tauri::Url) -> bool {
    let bundled = matches!(
        (url.scheme(), url.host_str()),
        ("tauri", Some("localhost")) | ("http", Some("tauri.localhost"))
    );
    #[cfg(debug_assertions)]
    let development =
        url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.port() == Some(1420);
    #[cfg(not(debug_assertions))]
    let development = false;
    bundled || development
}

/// 判断调用页面是否可以控制当前主窗口。
///
/// :param label: Tauri 注入的当前窗口标签。
/// :param url: 当前 WebView 的完整 URL。
/// :param adapter: 原生侧绑定精确 DSH 来源的适配器状态。
/// :return: 仅主窗口的内置页面、开发页或当前绑定 DSH 来源返回 `true`。
/// :raises: 适配器状态锁异常时远程来源失败关闭为 `false`。
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
    let result = execute_window_action(&window, action);
    if result.is_err() {
        crate::record_window_chrome_action_diagnostic(window.app_handle());
    }
    result
}

/// 返回不读取页面业务数据的固定自绘标题栏脚本。
///
/// :return: 可重复执行且只保留一个标题栏的静态 JavaScript。
/// :raises: 静态字符串读取不产生错误。
pub fn titlebar_script() -> &'static str {
    r#"(()=>{'use strict';
const B='dsh-desktop-titlebar',S='dsh-desktop-titlebar-style',C='control_main_window';
document.getElementById(B)?.remove();document.getElementById(S)?.remove();
const style=document.createElement('style');style.id=S;
style.textContent=':root{--dsh-desktop-titlebar-height:31px}body{box-sizing:border-box!important;height:100vh!important;padding-top:var(--dsh-desktop-titlebar-height)!important}#app,#root{height:100%!important;min-height:0!important}#dsh-desktop-titlebar{position:fixed;z-index:2147483647;top:0;right:0;left:0;height:31px;display:flex;align-items:center;color:var(--dsw-alias-label-secondary,#c9c9cc);font:12px/1 "Segoe UI",sans-serif;user-select:none;-webkit-user-select:none}#dsh-desktop-titlebar [data-title]{display:flex;align-items:center;gap:7px;padding-left:8px;pointer-events:none}#dsh-desktop-titlebar svg{width:15px;height:15px;color:#67c8ff}#dsh-desktop-titlebar [data-drag]{flex:1;align-self:stretch}#dsh-desktop-titlebar nav{display:flex!important;flex-direction:row!important;flex-wrap:nowrap!important;align-self:stretch;height:31px!important;margin-left:auto}#dsh-desktop-titlebar button{width:46px;height:31px;border:0;color:inherit;background:transparent;display:grid;place-items:center;padding:0;cursor:default}#dsh-desktop-titlebar button:hover,#dsh-desktop-titlebar button:focus-visible{background:rgb(255 255 255 / 12%);outline:none}#dsh-desktop-titlebar button[data-action=close]:hover{color:#fff;background:#c42b1c}#dsh-desktop-titlebar [data-restore]{display:none}#dsh-desktop-titlebar[data-maximized=true] [data-maximize]{display:none}#dsh-desktop-titlebar[data-maximized=true] [data-restore]{display:block}';
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
const applyState=(state)=>{if(!state||typeof state.maximized!=='boolean')return;bar.setAttribute('data-maximized',String(state.maximized));toggle.setAttribute('aria-label',state.maximized?'还原':'最大化')};
const invoke=(action)=>{const api=globalThis.__TAURI_INTERNALS__;if(typeof api?.invoke!=='function')return;try{const result=api.invoke(C,{action});if(result&&typeof result.then==='function')void result.then(applyState).catch(()=>{})}catch(_error){}};
bar.addEventListener('pointerdown',(event)=>{if(event.button===0&&!event.target?.closest?.('button'))invoke('start_dragging')});
bar.addEventListener('dblclick',(event)=>{if(event.button===0&&!event.target?.closest?.('button'))invoke('toggle_maximize')});
bar.addEventListener('click',(event)=>{const button=event.target?.closest?.('button[data-action]');if(button&&bar.contains(button))invoke(button.dataset.action)});
})();"#
}

/// 生成只更新桌面壳标题栏最大化状态的固定值脚本。
///
/// :param maximized: 原生窗口当前是否最大化。
/// :return: 只访问桌面壳标题栏固定 ID 的 JavaScript。
/// :raises: 布尔值格式化不产生错误。
pub fn sync_maximized_script(maximized: bool) -> String {
    format!(
        "(()=>{{const bar=document.getElementById('{TITLEBAR_ID}');if(!bar)return;bar.setAttribute('data-maximized','{maximized}');const button=bar.querySelector('[data-action=toggle_maximize]');button?.setAttribute('aria-label','{}')}})();",
        if maximized { "还原" } else { "最大化" }
    )
}

/// 在主 WebView 当前页面注入自绘标题栏并同步最大化状态。
///
/// :param webview: Tauri 页面加载回调提供的当前 WebView。
/// :return: 非主窗口或两段脚本均排入成功时返回 `Ok(())`。
/// :raises: 窗口状态查询或脚本注入失败时返回固定单位错误。
pub(crate) fn apply_to_main(webview: &Webview) -> Result<(), ()> {
    if webview.label() != "main" {
        return Ok(());
    }
    webview.eval(titlebar_script()).map_err(|_| ())?;
    let maximized = webview.window().is_maximized().map_err(|_| ())?;
    webview
        .eval(sync_maximized_script(maximized))
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{
        MainWindowAction, WindowControl, execute_window_action, main_window_control_allowed,
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
        fn start_dragging(&self) -> Result<(), ()> {
            self.dragging.set(true);
            Ok(())
        }

        fn minimize(&self) -> Result<(), ()> {
            self.minimized.set(true);
            Ok(())
        }

        fn is_maximized(&self) -> Result<bool, ()> {
            Ok(self.maximized.get())
        }

        fn maximize(&self) -> Result<(), ()> {
            self.maximized.set(true);
            Ok(())
        }

        fn unmaximize(&self) -> Result<(), ()> {
            self.maximized.set(false);
            Ok(())
        }

        fn close(&self) -> Result<(), ()> {
            self.closed.set(true);
            Ok(())
        }
    }

    #[test]
    fn toggle_maximize_changes_real_window_state_in_both_directions() {
        let window = FakeWindow::default();

        let maximized =
            execute_window_action(&window, MainWindowAction::ToggleMaximize).expect("maximize");
        assert!(window.maximized.get());
        assert!(maximized.maximized);

        let restored =
            execute_window_action(&window, MainWindowAction::ToggleMaximize).expect("restore");
        assert!(!window.maximized.get());
        assert!(!restored.maximized);
    }

    #[test]
    fn close_uses_the_window_close_path_without_process_exit() {
        let window = FakeWindow::default();

        execute_window_action(&window, MainWindowAction::Close).expect("close");

        assert!(window.closed.get());
    }

    #[test]
    fn remote_control_requires_the_current_bound_numeric_origin() {
        let controller = SkinAdapterController::default();
        let official = tauri::Url::parse("http://127.0.0.1:43127/chat").expect("official url");
        let other = tauri::Url::parse("http://127.0.0.1:43128/chat").expect("other url");
        assert!(controller.bind_navigation(
            &Version::parse("0.1.1-rc.2").expect("version"),
            RuntimeSkinCompatibility::Verified,
            &official,
        ));

        assert!(main_window_control_allowed("main", &official, &controller));
        assert!(!main_window_control_allowed("main", &other, &controller));
    }
}
