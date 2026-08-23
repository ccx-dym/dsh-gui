use std::{sync::Arc, time::Duration};

use dsh_desktop_lib::{
    diagnostics::{DiagnosticContext, TraceKind},
    network_proxy::{current_user_proxy, parse_proxy_server},
    update::{
        manifest::ManifestVerifier,
        version_source::{
            CompatibilitySource, ReqwestSourceTransport, SignedCompatibilitySource, SourcePolicy,
        },
    },
};
use semver::Version;
use url::Url;

#[test]
fn global_windows_proxy_is_normalized_for_https_requests() {
    let proxy = parse_proxy_server("127.0.0.1:7890").expect("global proxy");

    assert_eq!(proxy.as_str(), "http://127.0.0.1:7890/");
}

#[test]
fn https_specific_proxy_wins_over_http_and_unknown_entries() {
    let proxy =
        parse_proxy_server("ftp=ignored.example:21;http=127.0.0.1:8080;https=localhost:8443")
            .expect("https proxy");

    assert_eq!(proxy.as_str(), "http://localhost:8443/");
}

#[test]
fn unsafe_or_ambiguous_proxy_values_fail_closed() {
    for value in [
        "",
        "http=user:secret@127.0.0.1:7890",
        "https=file:///C:/proxy",
        "https=127.0.0.1:7890/path",
        "https=127.0.0.1:7890?token=value",
        "https=127.0.0.1:7890#fragment",
        "https=127.0.0.1",
    ] {
        assert!(parse_proxy_server(value).is_none(), "accepted {value}");
    }
}

#[tokio::test]
#[ignore = "需要当前用户代理和公开的线上发布通道"]
async fn configured_windows_proxy_reaches_and_verifies_live_runtime_channel() {
    assert!(
        current_user_proxy().is_some(),
        "test requires an explicit user proxy"
    );
    let public_key = std::env::var("DSH_LIVE_RUNTIME_PUBLIC_KEY").expect("public key");
    let transport =
        Arc::new(ReqwestSourceTransport::new(Duration::from_secs(5)).expect("source transport"));
    let source = SignedCompatibilitySource::new(
        Url::parse("https://raw.githubusercontent.com/ccx-dym/dsh-gui/main/releases/runtime/stable/manifest.json").unwrap(),
        Url::parse("https://raw.githubusercontent.com/ccx-dym/dsh-gui/main/releases/runtime/stable/manifest.sig").unwrap(),
        transport,
        SourcePolicy {
            request_timeout: Duration::from_secs(10),
            max_response_bytes: 1024 * 1024,
            max_retries: 0,
            retry_backoff: Duration::ZERO,
        },
        ManifestVerifier::new(&public_key, Version::parse("0.1.4").unwrap()).unwrap(),
    )
    .expect("compatibility source");

    let verified = source
        .latest_compatible_with_context(&DiagnosticContext::noop(TraceKind::Update))
        .await
        .expect("live runtime channel")
        .expect("published manifest");
    assert_eq!(verified.manifest.dsh_version.to_string(), "0.1.1-rc.2");
}
