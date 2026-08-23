use super::{MaskTone, SkinController, SkinFit, SkinPosition, SkinSettings};
use crate::runtime::install_state::{InstalledRuntime, RuntimeSkinCompatibility};
use semver::Version;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, State, Webview};

pub const DSH_ADAPTER_V1: &str = "dsh-0.1.1-rc.2-v1";
const VERIFIED_DSH_VERSION: &str = "0.1.1-rc.2";
const VERIFIED_NODE_VERSION: &str = "24.15.0";
const REVIEWED_UNVERIFIED_MANIFEST_DIGEST: &str =
    "61f98dda4c1bde5a76eb94837f1a9ca00ade9620fe4668329bbab3b0d0fb79c4";
const STYLE_ID: &str = "dsh-desktop-skin-style";
const BACKGROUND_ID: &str = "dsh-desktop-skin-background";

/// 与一个经过人工验证的 DSH DOM 合约绑定的适配器标识。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DshAdapter {
    version: &'static str,
}

/// 皮肤因当前 runtime 不满足验证边界而被关闭的稳定原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkinDisableReason {
    VersionUnverified,
}

/// 活动 runtime 是否允许注入皮肤的失败关闭策略。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SkinRuntimePolicy {
    pub enabled: bool,
    pub reason: Option<SkinDisableReason>,
}

impl DshAdapter {
    /// 返回同时绑定 DSH 版本和 DOM 合约的固定适配器版本。
    ///
    /// :return: 不含用户输入的静态适配器标识。
    /// :raises: 值来自封闭 allowlist，不产生错误。
    pub fn version(self) -> &'static str {
        self.version
    }
}

/// 为精确验证过的 DSH 版本选择适配器。
///
/// :param version: 已激活部署中经过清单验证的严格 semver。
/// :return: 精确匹配时返回适配器，其他版本返回 `None` 并失败关闭。
/// :raises: 只比较类型化版本，不产生错误。
pub fn adapter_for(version: &Version) -> Option<DshAdapter> {
    (version.to_string() == VERIFIED_DSH_VERSION).then_some(DshAdapter {
        version: DSH_ADAPTER_V1,
    })
}

/// 组合签名清单结论与精确 DOM 适配器，决定是否允许皮肤注入。
///
/// :param version: 权威活动部署中的严格 DSH 版本。
/// :param status: 已签名兼容清单给出的皮肤验证结论。
/// :return: 仅两道门禁都通过时启用；否则返回稳定关闭原因。
/// :raises: 只比较类型化值和封闭 allowlist，不产生错误。
pub fn skin_runtime_policy(
    version: &Version,
    status: RuntimeSkinCompatibility,
) -> SkinRuntimePolicy {
    let enabled = status == RuntimeSkinCompatibility::Verified && adapter_for(version).is_some();
    SkinRuntimePolicy {
        enabled,
        reason: (!enabled).then_some(SkinDisableReason::VersionUnverified),
    }
}

/// 把已随桌面客户端复核的历史运行时清单提升为等效皮肤授权。
///
/// 首版 `0.1.1-rc.2` 清单在真实 DOM 验证前以 `unverified` 发布。桌面客户端
/// 只能对版本、Node 与原始签名清单摘要全部精确匹配的既有部署提升权限；其他
/// 未验证部署继续失败关闭，避免仅凭相同 semver 扩大授权范围。
///
/// :param runtime: 从权威 deployment pointer 读取的完整运行时描述符。
/// :return: 精确命中已复核描述符时返回 `Verified`，否则保留原签名结论。
/// :raises: 只比较已解析版本和规范摘要，不产生错误。
pub(crate) fn effective_skin_compatibility(runtime: &InstalledRuntime) -> RuntimeSkinCompatibility {
    if runtime.skin_compatibility == RuntimeSkinCompatibility::Unverified
        && runtime.version.to_string() == VERIFIED_DSH_VERSION
        && runtime.node_version.to_string() == VERIFIED_NODE_VERSION
        && runtime.manifest_digest == REVIEWED_UNVERIFIED_MANIFEST_DIGEST
    {
        RuntimeSkinCompatibility::Verified
    } else {
        runtime.skin_compatibility
    }
}

/// 构造不读取网络、存储或业务 DOM 的 v1 注入脚本。
///
/// :param settings: 已由 `SkinStore` 验证的当前设置。
/// :return: 仅启用且摘要规范时返回脚本，否则返回 `None` 交由调用方清理。
/// :raises: 所有动态值均来自封闭枚举、整数边界和规范摘要，不传播错误正文。
pub fn adapter_script(settings: &SkinSettings) -> Option<String> {
    adapter_script_for_page(settings, 0)
}

fn adapter_script_for_page(settings: &SkinSettings, page_token: u64) -> Option<String> {
    if !settings.immersive {
        return None;
    }
    let digest = settings.image_digest.as_deref()?;
    if !is_canonical_digest(digest) {
        return None;
    }

    let background_size = match settings.fit {
        SkinFit::Cover => "cover",
        SkinFit::Contain => "contain",
        SkinFit::Stretch => "100% 100%",
        SkinFit::Center => "auto",
    };
    let background_position = match settings.position {
        SkinPosition::TopLeft => "left top",
        SkinPosition::Top => "center top",
        SkinPosition::TopRight => "right top",
        SkinPosition::Left => "left center",
        SkinPosition::Center => "center center",
        SkinPosition::Right => "right center",
        SkinPosition::BottomLeft => "left bottom",
        SkinPosition::Bottom => "center bottom",
        SkinPosition::BottomRight => "right bottom",
    };
    let (surface_rgb, mask_rgb) = match settings.mask_tone {
        MaskTone::Light => ("255,255,255", "255,255,255"),
        MaskTone::Dark => ("22,28,38", "10,16,26"),
    };
    let panel_opacity = f32::from(settings.panel_opacity_percent) / 100.0;
    let mask_opacity = f32::from(settings.mask_opacity_percent) / 100.0;
    let blur_scale = 1.0 + f32::from(settings.blur_px) / 500.0;

    // 脚本只插入桌面壳自有节点；DOM 合约和三个计算变量任一缺失都会先清理再报告失败。
    Some(format!(
        r#"(()=>{{'use strict';const A='{adapter}',T={page_token},S='{style_id}',B='{background_id}';const clean=()=>{{document.getElementById(S)?.remove();document.getElementById(B)?.remove();}};const report=(compatible)=>{{const invoke=globalThis.__TAURI_INTERNALS__?.invoke;if(typeof invoke==='function'){{const result=invoke('report_skin_adapter',{{adapterVersion:A,pageToken:T,compatible}});if(result&&typeof result.catch==='function'){{void result.catch(()=>{{}});}}}}}};try{{clean();const root=document.querySelector('#root');const css=root?getComputedStyle(root):null;const vars=['--dsw-alias-bg-base','--dsw-alias-bg-layer-1','--dsw-alias-bg-layer-2'];const originOk=location.protocol==='http:'&&location.hostname==='127.0.0.1'&&location.port!=='';if(!originOk||!root||!css||vars.some((name)=>css.getPropertyValue(name).trim()==='')){{report(false);return;}}const bg=document.createElement('div');bg.id=B;bg.setAttribute('aria-hidden','true');bg.style.cssText='position:fixed;inset:0;z-index:-2147483647;pointer-events:none;background-image:linear-gradient(rgba({mask_rgb},{mask_opacity:.2}),rgba({mask_rgb},{mask_opacity:.2})),url("dsh-skin://localhost/{digest}");background-repeat:no-repeat;background-size:{background_size};background-position:{background_position};filter:blur({blur}px);transform:scale({blur_scale:.3});transform-origin:center;will-change:transform';const style=document.createElement('style');style.id=S;style.textContent='html,body,#root{{background:transparent !important}}:root{{--dsw-alias-bg-base:rgba({surface_rgb},{panel_opacity:.2});--dsw-alias-bg-layer-1:rgba({surface_rgb},{panel_opacity:.2});--dsw-alias-bg-layer-2:rgba({surface_rgb},{panel_opacity:.2});--dsh-desktop-border-opacity:{border_opacity:.2}}}';document.documentElement.prepend(bg);document.head.append(style);report(true);}}catch(_error){{try{{clean();}}catch(_cleanupError){{}}try{{report(false);}}catch(_reportError){{}}}}}})();"#,
        adapter = DSH_ADAPTER_V1,
        page_token = page_token,
        style_id = STYLE_ID,
        background_id = BACKGROUND_ID,
        mask_rgb = mask_rgb,
        mask_opacity = mask_opacity,
        digest = digest,
        background_size = background_size,
        background_position = background_position,
        blur = settings.blur_px,
        blur_scale = blur_scale,
        surface_rgb = surface_rgb,
        panel_opacity = panel_opacity,
        border_opacity = panel_opacity,
    ))
}

/// 构造仅撤销桌面壳自有节点的固定清理脚本。
///
/// :return: 不含协议 URL、用户输入或 DSH 业务选择器的静态脚本。
/// :raises: 静态字符串构造不产生错误。
pub fn cleanup_script() -> &'static str {
    "(()=>{'use strict';document.getElementById('dsh-desktop-skin-style')?.remove();document.getElementById('dsh-desktop-skin-background')?.remove();})();"
}

/// 根据可信部署版本和已完成导航 URL 生成应用或清理脚本。
///
/// :param version: 权威激活部署中读取的严格 DSH 版本。
/// :param skin_compatibility: 与该部署持久绑定的 signed 皮肤验证结论。
/// :param url: WebView2 已完成加载的页面 URL。
/// :param settings: 当前已提交皮肤设置。
/// :return: 只有精确版本、数字回环来源和有效设置同时满足时返回注入脚本。
/// :raises: URL 或设置不满足合约时返回固定清理脚本，不传播动态正文。
pub fn page_script(
    version: &Version,
    skin_compatibility: RuntimeSkinCompatibility,
    url: &tauri::Url,
    settings: &SkinSettings,
) -> String {
    let trusted_origin = url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.port().is_some_and(|port| port != 1420)
        && url.username().is_empty()
        && url.password().is_none();
    if !trusted_origin || !skin_runtime_policy(version, skin_compatibility).enabled {
        return cleanup_script().to_owned();
    }
    adapter_script(settings).unwrap_or_else(|| cleanup_script().to_owned())
}

fn is_canonical_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Compatibility {
    Pending,
    Compatible,
    Incompatible,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveAdapter {
    adapter: DshAdapter,
    origin: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct AdapterState {
    active: Option<ActiveAdapter>,
    compatibility: Option<Compatibility>,
    epoch: u64,
    page_token: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SkinPageBinding {
    token: u64,
}

impl SkinPageBinding {
    /// 返回本次已完成导航的一次性报告令牌。
    ///
    /// :return: 只在原生门禁内单调生成的代次值。
    /// :raises: 值对象读取不产生错误。
    pub(crate) fn token(self) -> u64 {
        self.token
    }
}

/// 保存当前可信部署适配器和当前页面 DOM 报告的进程内门禁。
#[derive(Clone, Debug, Default)]
pub struct SkinAdapterController {
    state: Arc<Mutex<AdapterState>>,
}

/// 官方页面收到的固定、无数据读取能力的报告响应。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkinAdapterReport {
    pub accepted: bool,
    pub compatible: bool,
}

impl SkinAdapterController {
    /// 将门禁绑定到权威激活部署的精确 DSH 版本。
    ///
    /// :param version: 已验证激活部署的 DSH 版本。
    /// :param skin_compatibility: 与该部署共同加载的 signed 皮肤验证结论。
    /// :param url: Ready 事件经过严格解析的数字回环 URL。
    /// :return: 是否存在精确适配器。
    /// :raises: 锁中毒时失败关闭并返回 `false`。
    pub(crate) fn bind_navigation(
        &self,
        version: &Version,
        skin_compatibility: RuntimeSkinCompatibility,
        url: &tauri::Url,
    ) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if !skin_runtime_policy(version, skin_compatibility).enabled {
            invalidate_state(&mut state);
            return false;
        }
        let Some(adapter) = adapter_for(version) else {
            invalidate_state(&mut state);
            return false;
        };
        let Some(origin) = numeric_loopback_origin(url) else {
            invalidate_state(&mut state);
            return false;
        };
        if advance_epoch(&mut state).is_none() {
            invalidate_state(&mut state);
            return false;
        }
        state.active = Some(ActiveAdapter { adapter, origin });
        state.compatibility = Some(Compatibility::Pending);
        state.page_token = None;
        true
    }

    /// 清除当前部署和页面兼容状态。
    ///
    /// :return: 无返回数据。
    /// :raises: 锁中毒时保持失败关闭状态。
    pub(crate) fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            invalidate_state(&mut state);
        }
    }

    /// 在任一主窗口导航开始时立即使上一页面令牌失效。
    ///
    /// :param url: WebView2 报告的新导航 URL。
    /// :return: 无返回数据；跨来源会同时清除版本绑定。
    /// :raises: 锁异常时不建立任何新能力。
    pub(crate) fn navigation_started(&self, url: &tauri::Url) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let matches = numeric_loopback_origin(url).is_some_and(|origin| {
            state
                .active
                .as_ref()
                .is_some_and(|active| active.origin == origin)
        });
        if !matches || advance_epoch(&mut state).is_none() {
            invalidate_state(&mut state);
            return;
        }
        state.page_token = None;
        state.compatibility = Some(Compatibility::Pending);
    }

    /// 为一次已完成的官方页面加载重置 DOM 报告状态。
    ///
    /// :param url: WebView2 报告的已完成加载 URL。
    /// :return: 当前精确适配器；无适配器或锁异常时返回 `None`。
    /// :raises: 锁异常被折叠为失败关闭。
    pub(crate) fn begin_page(&self, url: &tauri::Url) -> Option<SkinPageBinding> {
        let mut state = self.state.lock().ok()?;
        let matches = numeric_loopback_origin(url).is_some_and(|origin| {
            state
                .active
                .as_ref()
                .is_some_and(|active| active.origin == origin)
        });
        if !matches {
            invalidate_state(&mut state);
            return None;
        }
        let has_adapter = state
            .active
            .as_ref()
            .is_some_and(|active| active.adapter.version() == DSH_ADAPTER_V1);
        if !has_adapter {
            invalidate_state(&mut state);
            return None;
        }
        let token = advance_epoch(&mut state)?;
        state.page_token = Some(token);
        state.compatibility = Some(Compatibility::Pending);
        Some(SkinPageBinding { token })
    }

    /// 接收官方主页面的 DOM 合约结果，不能建立新的版本能力。
    ///
    /// :param adapter_version: 注入脚本携带的固定适配器标识。
    /// :param compatible: 当前页面是否通过 DOM 合约检查。
    /// :return: 仅匹配原生已激活适配器时接受；返回值不包含设置或路径。
    /// :raises: 状态锁异常时返回未接受的失败关闭响应。
    pub fn report(
        &self,
        adapter_version: &str,
        page_token: u64,
        compatible: bool,
    ) -> SkinAdapterReport {
        let Ok(mut state) = self.state.lock() else {
            return SkinAdapterReport {
                accepted: false,
                compatible: false,
            };
        };
        let accepted = state
            .active
            .as_ref()
            .is_some_and(|active| active.adapter.version() == adapter_version)
            && state.page_token == Some(page_token);
        if accepted {
            // 每个完成导航只允许一次结果，防止页面在成功后重放相反报告改变状态。
            state.page_token.take();
            state.compatibility = Some(if compatible {
                Compatibility::Compatible
            } else {
                Compatibility::Incompatible
            });
        }
        SkinAdapterReport {
            accepted,
            compatible: accepted && compatible,
        }
    }
}

fn numeric_loopback_origin(url: &tauri::Url) -> Option<String> {
    let port = url.port().filter(|port| *port != 1420)?;
    (url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| format!("http://127.0.0.1:{port}"))
}

fn advance_epoch(state: &mut AdapterState) -> Option<u64> {
    let next = state.epoch.checked_add(1)?;
    state.epoch = next;
    Some(next)
}

fn invalidate_state(state: &mut AdapterState) {
    state.epoch = state.epoch.saturating_add(1);
    state.active = None;
    state.compatibility = None;
    state.page_token = None;
}

/// 接收官方页面的适配器 DOM 合约报告，不读取或修改皮肤设置。
///
/// :param app: 当前 Tauri 应用句柄，仅用于在失败报告后清理桌面节点。
/// :param controller: 原生侧预先绑定可信 DSH 版本的适配器门禁。
/// :param adapter_version: 注入脚本携带的固定适配器标识。
/// :param page_token: 原生侧为本次完成导航生成的一次性代次令牌。
/// :param compatible: 当前页面是否通过只读 DOM 合约检查。
/// :return: 不含设置、路径和诊断正文的固定报告响应。
/// :raises: 状态异常折叠为 `accepted=false`，不向页面返回动态错误。
#[tauri::command]
pub fn report_skin_adapter(
    app: AppHandle,
    controller: State<'_, SkinAdapterController>,
    adapter_version: String,
    page_token: u64,
    compatible: bool,
) -> SkinAdapterReport {
    let report = controller.report(&adapter_version, page_token, compatible);
    if report.accepted
        && !report.compatible
        && let Some(main) = app.get_webview_window("main")
        && main.eval(cleanup_script()).is_err()
    {
        crate::record_skin_apply_diagnostic(&app);
    }
    report
}

/// 在已完成加载的主 WebView 上应用当前适配器或固定清理脚本。
///
/// :param webview: Tauri 的单一主 WebView。
/// :param url: 本次完成加载的真实页面 URL。
/// :param adapter: 已由运行时部署版本预先绑定的原生适配器门禁。
/// :param skins: 只读加载当前已提交设置的皮肤控制器。
/// :return: 脚本成功排入 WebView2 时返回 `Ok(())`。
/// :raises: 来源、版本或设置不兼容时只清理；仅 eval 失败返回固定单位错误。
pub(crate) fn apply_to_main(
    webview: &Webview,
    url: &tauri::Url,
    adapter: &SkinAdapterController,
    skins: &SkinController,
) -> Result<(), ()> {
    if webview.label() != "main" {
        return Ok(());
    }
    let Some(page) = adapter.begin_page(url) else {
        return webview.eval(cleanup_script()).map_err(|_| ());
    };
    let script = skins
        .load()
        .ok()
        .and_then(|state| adapter_script_for_page(&state.settings, page.token()))
        .unwrap_or_else(|| cleanup_script().to_owned());
    webview.eval(script).map_err(|_| ())
}

/// 保存或恢复默认成功后，将最新设置刷新到当前主窗口。
///
/// :param app: 当前 Tauri 应用句柄。
/// :param settings: 刚完成持久化的可信设置快照。
/// :return: 注入或清理脚本成功排入当前主窗口时返回 `Ok(())`。
/// :raises: 窗口、URL、门禁状态或 eval 不可用时只返回固定单位错误。
pub(crate) fn refresh_main_skin(app: &AppHandle, settings: &SkinSettings) -> Result<(), ()> {
    let main = app.get_webview_window("main").ok_or(())?;
    let adapter = app.try_state::<SkinAdapterController>().ok_or(())?;
    let url = main.url().map_err(|_| ())?;
    let script = adapter
        .begin_page(&url)
        .and_then(|page| adapter_script_for_page(settings, page.token()))
        .unwrap_or_else(|| cleanup_script().to_owned());
    main.eval(script).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{DSH_ADAPTER_V1, SkinAdapterController};
    use crate::runtime::install_state::RuntimeSkinCompatibility;
    use semver::Version;

    #[test]
    fn remote_report_cannot_grant_an_unknown_native_version() {
        let controller = SkinAdapterController::default();
        let url = tauri::Url::parse("http://127.0.0.1:43127/chat").expect("url");
        assert!(!controller.bind_navigation(
            &Version::parse("0.1.2").expect("version"),
            RuntimeSkinCompatibility::Verified,
            &url
        ));

        let report = controller.report(DSH_ADAPTER_V1, 1, true);

        assert!(!report.accepted);
        assert!(!report.compatible);
    }

    #[test]
    fn matching_false_report_disables_the_current_page_without_granting_capabilities() {
        let controller = SkinAdapterController::default();
        let url = tauri::Url::parse("http://127.0.0.1:43127/chat").expect("url");
        assert!(controller.bind_navigation(
            &Version::parse("0.1.1-rc.2").expect("version"),
            RuntimeSkinCompatibility::Verified,
            &url
        ));
        let page = controller.begin_page(&url).expect("page binding");
        assert!(
            !controller
                .report("dsh-unknown-v1", page.token(), true)
                .accepted
        );
        let report = controller.report(DSH_ADAPTER_V1, page.token(), false);

        assert!(report.accepted);
        assert!(!report.compatible);
    }

    #[test]
    fn finished_page_must_match_the_bound_exact_origin_and_navigation_epoch() {
        let controller = SkinAdapterController::default();
        let version = Version::parse("0.1.1-rc.2").expect("version");
        let official = tauri::Url::parse("http://127.0.0.1:43127/chat").expect("url");
        let other_port = tauri::Url::parse("http://127.0.0.1:43128/chat").expect("url");
        assert!(controller.bind_navigation(
            &version,
            RuntimeSkinCompatibility::Verified,
            &official
        ));
        assert!(controller.begin_page(&other_port).is_none());

        assert!(controller.bind_navigation(
            &version,
            RuntimeSkinCompatibility::Verified,
            &official
        ));
        let first = controller.begin_page(&official).expect("first page");
        controller.navigation_started(&official);
        let second = controller.begin_page(&official).expect("second page");
        assert_ne!(first.token(), second.token());
        assert!(
            !controller
                .report(DSH_ADAPTER_V1, first.token(), true)
                .accepted
        );
        assert!(
            controller
                .report(DSH_ADAPTER_V1, second.token(), true)
                .accepted
        );

        controller.navigation_started(&other_port);
        assert!(controller.begin_page(&official).is_none());
        assert!(
            !controller
                .report(DSH_ADAPTER_V1, second.token(), true)
                .accepted
        );
    }

    #[test]
    fn page_report_token_is_consumed_after_the_first_matching_report() {
        let controller = SkinAdapterController::default();
        let version = Version::parse("0.1.1-rc.2").expect("version");
        let official = tauri::Url::parse("http://127.0.0.1:43127/chat").expect("url");
        assert!(controller.bind_navigation(
            &version,
            RuntimeSkinCompatibility::Verified,
            &official
        ));
        let page = controller.begin_page(&official).expect("page");

        assert!(
            controller
                .report(DSH_ADAPTER_V1, page.token(), true)
                .accepted
        );
        assert!(
            !controller
                .report(DSH_ADAPTER_V1, page.token(), false)
                .accepted
        );
    }
}
