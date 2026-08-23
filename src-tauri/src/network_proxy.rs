use url::Url;

/// 解析 Windows `ProxyServer` 中适用于 HTTPS 目标的显式代理。
///
/// 支持全局 `host:port` 和按协议分隔的 `http=...;https=...` 形式。代理本身允许
/// HTTP 或 HTTPS，但拒绝凭据、路径、查询参数和缺失端口，避免把不透明系统值直接
/// 交给网络客户端。
///
/// :param value: Windows 当前用户代理字符串。
/// :return: 可安全交给 Reqwest/Tauri updater 的代理 URL；无适用值时返回 `None`。
/// :raises: 解析失败通过 `None` 表示，不抛出错误。
pub fn parse_proxy_server(value: &str) -> Option<Url> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let selected = if value.contains('=') {
        let mut http = None;
        let mut https = None;
        for entry in value
            .split(';')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let (scheme, endpoint) = entry.split_once('=')?;
            match scheme.trim().to_ascii_lowercase().as_str() {
                "http" => http = Some(endpoint.trim()),
                "https" => https = Some(endpoint.trim()),
                _ => {}
            }
        }
        https.or(http)?
    } else {
        value
    };
    let candidate = if selected.contains("://") {
        selected.to_owned()
    } else {
        format!("http://{selected}")
    };
    let url = Url::parse(&candidate).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(url)
}

/// 读取当前交互式 Windows 用户的显式系统代理。
///
/// PAC/WPAD 配置不会被猜测或执行；没有安全的显式代理时调用方继续使用默认直连。
/// 返回值不得写入日志，避免泄露企业网络拓扑。
///
/// :return: 当前用户的安全显式代理 URL，或 `None`。
/// :raises: WinHTTP 读取、字符串转换或校验失败均安全返回 `None`。
#[cfg(windows)]
pub fn current_user_proxy() -> Option<Url> {
    use windows::Win32::{
        Foundation::{GlobalFree, HGLOBAL},
        Networking::WinHttp::{
            WINHTTP_CURRENT_USER_IE_PROXY_CONFIG, WinHttpGetIEProxyConfigForCurrentUser,
        },
    };

    let mut config = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
    // SAFETY: WinHTTP 初始化调用方拥有的结构；返回的三个字符串都按文档使用
    // GlobalFree 释放，并且只在释放前复制为 Rust String。
    unsafe { WinHttpGetIEProxyConfigForCurrentUser(&mut config).ok()? };
    let proxy = if config.lpszProxy.is_null() {
        None
    } else {
        // SAFETY: 成功的 WinHTTP 调用返回以 NUL 结尾的当前进程可读 UTF-16 字符串。
        unsafe { config.lpszProxy.to_string().ok() }
    };
    for pointer in [
        config.lpszAutoConfigUrl.0,
        config.lpszProxy.0,
        config.lpszProxyBypass.0,
    ] {
        if !pointer.is_null() {
            // SAFETY: 每个非空指针均由同一次 WinHTTP 调用分配，且在此仅释放一次。
            let _ = unsafe { GlobalFree(Some(HGLOBAL(pointer.cast()))) };
        }
    }
    proxy.as_deref().and_then(parse_proxy_server)
}

#[cfg(not(windows))]
pub fn current_user_proxy() -> Option<Url> {
    None
}

/// 创建继承当前用户显式 Windows 代理的 Reqwest builder。
///
/// :return: 尚未设置调用方超时和 HTTPS 策略的 builder。
/// :raises reqwest::Error: 系统代理无法转换为 Reqwest 代理时返回。
pub(crate) fn reqwest_client_builder() -> Result<reqwest::ClientBuilder, reqwest::Error> {
    let builder = reqwest::Client::builder();
    match current_user_proxy() {
        Some(proxy) => Ok(builder.proxy(reqwest::Proxy::all(proxy)?)),
        None => Ok(builder),
    }
}
