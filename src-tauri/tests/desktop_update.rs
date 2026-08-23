use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use dsh_desktop_lib::{
    desktop_update::{DesktopUpdateBackend, DesktopUpdateController, DesktopUpdateError},
    domain::{DesktopRelease, DesktopUpdateErrorKind, DesktopUpdateState},
};
use semver::Version;
use tokio::sync::Notify;

#[derive(Clone)]
struct FakeBackend {
    check_result: Result<Option<DesktopRelease>, DesktopUpdateError>,
    install_result: Result<(), DesktopUpdateError>,
    check_gate: Option<Arc<Notify>>,
    check_started: Option<Arc<Notify>>,
    installed_releases: Arc<Mutex<Vec<DesktopRelease>>>,
}

impl FakeBackend {
    fn available(version: &str) -> Self {
        Self {
            check_result: Ok(Some(DesktopRelease {
                version: Version::parse(version).expect("测试版本必须有效"),
                notes: Some("安全更新".to_owned()),
                published_at: Some("2026-08-23T00:00:00Z".to_owned()),
            })),
            install_result: Ok(()),
            check_gate: None,
            check_started: None,
            installed_releases: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn failing(error: DesktopUpdateError) -> Self {
        Self {
            check_result: Err(error),
            install_result: Ok(()),
            check_gate: None,
            check_started: None,
            installed_releases: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn available_with_notes(version: &str, notes: String) -> Self {
        let mut backend = Self::available(version);
        backend.check_result = Ok(Some(DesktopRelease {
            version: Version::parse(version).expect("测试版本必须有效"),
            notes: Some(notes),
            published_at: None,
        }));
        backend
    }
}

impl DesktopUpdateBackend for FakeBackend {
    fn check<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<DesktopRelease>, DesktopUpdateError>> + Send + 'a>>
    {
        Box::pin(async move {
            if let Some(started) = &self.check_started {
                started.notify_one();
            }
            if let Some(gate) = &self.check_gate {
                gate.notified().await;
            }
            self.check_result.clone()
        })
    }

    fn install<'a>(
        &'a self,
        release: DesktopRelease,
    ) -> Pin<Box<dyn Future<Output = Result<(), DesktopUpdateError>> + Send + 'a>> {
        Box::pin(async move {
            self.installed_releases
                .lock()
                .expect("安装记录锁不应中毒")
                .push(release);
            self.install_result
        })
    }
}

fn current_version() -> Version {
    Version::parse("0.1.0").expect("当前桌面版本必须有效")
}

fn fixture_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间必须可用")
        .as_nanos();
    std::env::temp_dir().join(format!("dsh-desktop-update-{label}-{nonce}"))
}

fn write_guarded_files(root: &Path) -> (PathBuf, PathBuf, Vec<u8>, Vec<u8>) {
    let active_pointer = root.join("local/runtimes/active.json");
    let user_data = root.join("local/data/generation-user/config.json");
    std::fs::create_dir_all(active_pointer.parent().expect("active 必须有父目录")).unwrap();
    std::fs::create_dir_all(user_data.parent().expect("数据必须有父目录")).unwrap();
    let active_bytes = br#"{"version":"0.1.1-rc.2"}"#.to_vec();
    let data_bytes = br#"{"model":"user-choice"}"#.to_vec();
    std::fs::write(&active_pointer, &active_bytes).unwrap();
    std::fs::write(&user_data, &data_bytes).unwrap();
    (active_pointer, user_data, active_bytes, data_bytes)
}

#[tokio::test]
async fn desktop_check_persists_available_state_without_touching_runtime_or_user_data() {
    let root = fixture_root("check-isolation");
    let settings = root.join("settings");
    let (active_pointer, user_data, active_before, data_before) = write_guarded_files(&root);
    let controller = DesktopUpdateController::new(settings.clone(), current_version());

    let result = controller
        .check(&FakeBackend::available("0.1.1"))
        .await
        .expect("可用更新检查应成功");

    assert_eq!(result.revision, 2);
    assert!(matches!(
        result.state,
        DesktopUpdateState::Available { ref version, .. } if version == "0.1.1"
    ));
    assert_eq!(std::fs::read(active_pointer).unwrap(), active_before);
    assert_eq!(std::fs::read(user_data).unwrap(), data_before);
    let persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(settings.join("desktop-update-state.json")).unwrap())
            .expect("连续状态替换后必须保留完整 JSON");
    assert_eq!(persisted["revision"], 2);
    assert_eq!(persisted["state"]["phase"], "available");
}

#[tokio::test]
async fn backend_failures_publish_only_fixed_error_categories() {
    for (label, error, expected) in [
        (
            "offline",
            DesktopUpdateError::Offline,
            DesktopUpdateErrorKind::Offline,
        ),
        (
            "metadata",
            DesktopUpdateError::InvalidMetadata,
            DesktopUpdateErrorKind::InvalidMetadata,
        ),
        (
            "signature",
            DesktopUpdateError::SignatureInvalid,
            DesktopUpdateErrorKind::SignatureInvalid,
        ),
    ] {
        let controller =
            DesktopUpdateController::new(fixture_root(label).join("settings"), current_version());
        assert_eq!(
            controller.check(&FakeBackend::failing(error)).await,
            Err(error)
        );
        let snapshot = controller.snapshot().await;
        assert!(matches!(
            snapshot.state,
            DesktopUpdateState::Failed { error_kind } if error_kind == expected
        ));
        let json = serde_json::to_value(snapshot).expect("状态必须可序列化");
        assert_eq!(
            json["state"]["errorKind"],
            serde_json::to_value(expected).unwrap()
        );
    }
}

#[tokio::test]
async fn concurrent_check_fails_closed_without_overwriting_in_flight_state() {
    let gate = Arc::new(Notify::new());
    let started = Arc::new(Notify::new());
    let backend = FakeBackend {
        check_gate: Some(gate.clone()),
        check_started: Some(started.clone()),
        ..FakeBackend::available("0.1.1")
    };
    let controller = Arc::new(DesktopUpdateController::new(
        fixture_root("concurrent").join("settings"),
        current_version(),
    ));
    let mut worker = {
        let controller = controller.clone();
        let backend = backend.clone();
        tokio::spawn(async move { controller.check(&backend).await })
    };
    // 全量测试会并发执行大量 Windows 文件安全夹具；给同步落盘留出足够余量，
    // 但仍以有限超时证明检查不会永久卡在进入 backend 之前。
    tokio::select! {
        _ = started.notified() => {}
        result = &mut worker => panic!("首个检查进入 backend 前结束: {result:?}"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
            panic!("首个检查必须进入 backend")
        }
    }
    assert!(matches!(
        controller.snapshot().await.state,
        DesktopUpdateState::Checking
    ));

    assert_eq!(
        controller.check(&FakeBackend::available("0.1.2")).await,
        Err(DesktopUpdateError::InstallFailed)
    );
    assert!(matches!(
        controller.snapshot().await.state,
        DesktopUpdateState::Checking
    ));

    gate.notify_one();
    let completed = worker.await.unwrap().unwrap();
    assert!(matches!(
        completed.state,
        DesktopUpdateState::Available { ref version, .. } if version == "0.1.1"
    ));
}

#[tokio::test]
async fn install_uses_selected_release_and_preserves_runtime_and_user_data() {
    let root = fixture_root("install-isolation");
    let (active_pointer, user_data, active_before, data_before) = write_guarded_files(&root);
    let controller = DesktopUpdateController::new(root.join("settings"), current_version());
    let backend = FakeBackend::available("0.1.1");
    let available = controller.check(&backend).await.unwrap();

    let installed = controller
        .install(available.revision, &backend)
        .await
        .expect("安装后端成功时应进入 installing");

    assert_eq!(installed.revision, 4);
    assert!(matches!(
        installed.state,
        DesktopUpdateState::Installing { ref version } if version == "0.1.1"
    ));
    assert_eq!(
        *backend
            .installed_releases
            .lock()
            .expect("安装记录锁不应中毒"),
        vec![DesktopRelease {
            version: Version::parse("0.1.1").unwrap(),
            notes: Some("安全更新".to_owned()),
            published_at: Some("2026-08-23T00:00:00Z".to_owned()),
        }]
    );
    assert_eq!(std::fs::read(active_pointer).unwrap(), active_before);
    assert_eq!(std::fs::read(user_data).unwrap(), data_before);
}

#[tokio::test]
async fn stale_or_failed_install_is_fail_closed() {
    let controller = DesktopUpdateController::new(
        fixture_root("install-failure").join("settings"),
        current_version(),
    );
    let backend = FakeBackend::available("0.1.1");
    let available = controller.check(&backend).await.unwrap();

    assert_eq!(
        controller.install(available.revision - 1, &backend).await,
        Err(DesktopUpdateError::InstallFailed)
    );
    assert_eq!(controller.snapshot().await, available);

    let failing = FakeBackend {
        install_result: Err(DesktopUpdateError::SignatureInvalid),
        ..backend
    };
    assert_eq!(
        controller.install(available.revision, &failing).await,
        Err(DesktopUpdateError::SignatureInvalid)
    );
    assert!(matches!(
        controller.snapshot().await.state,
        DesktopUpdateState::Failed {
            error_kind: DesktopUpdateErrorKind::SignatureInvalid
        }
    ));
}

#[tokio::test]
async fn replay_or_downgrade_release_is_rejected_before_becoming_installable() {
    for candidate in ["0.1.0", "0.0.9"] {
        let controller = DesktopUpdateController::new(
            fixture_root(candidate).join("settings"),
            current_version(),
        );

        assert_eq!(
            controller.check(&FakeBackend::available(candidate)).await,
            Err(DesktopUpdateError::InvalidMetadata)
        );
        assert!(matches!(
            controller.snapshot().await.state,
            DesktopUpdateState::Failed {
                error_kind: DesktopUpdateErrorKind::InvalidMetadata
            }
        ));
    }
}

#[tokio::test]
async fn build_metadata_does_not_make_equal_precedence_release_installable() {
    let controller = DesktopUpdateController::new(
        fixture_root("build-metadata").join("settings"),
        Version::parse("0.1.0+installed").unwrap(),
    );

    assert_eq!(
        controller
            .check(&FakeBackend::available("0.1.0+remote"))
            .await,
        Err(DesktopUpdateError::InvalidMetadata)
    );
}

#[tokio::test]
async fn oversized_state_is_rejected_before_a_temporary_file_is_written() {
    let settings = fixture_root("oversized").join("settings");
    let controller = DesktopUpdateController::new(settings.clone(), current_version());

    assert_eq!(
        controller
            .check(&FakeBackend::available_with_notes(
                "0.1.1",
                "x".repeat(70 * 1024)
            ))
            .await,
        Err(DesktopUpdateError::InstallFailed)
    );
    let temporary_files = std::fs::read_dir(&settings)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .count();
    assert_eq!(temporary_files, 0, "超限状态不得先写临时文件");
}

#[cfg(windows)]
#[tokio::test]
async fn repeated_failed_persistence_reuses_one_bounded_temporary_slot() {
    use std::os::windows::fs::OpenOptionsExt;

    let settings = fixture_root("bounded-temp").join("settings");
    std::fs::create_dir_all(&settings).unwrap();
    let controller = DesktopUpdateController::new(settings.clone(), current_version());
    let state_path = settings.join("desktop-update-state.json");
    std::fs::write(&state_path, b"locked-state").unwrap();
    let _replacement_blocker = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&state_path)
        .expect("测试句柄应拒绝目标替换");

    for _ in 0..2 {
        assert_eq!(
            controller.check(&FakeBackend::available("0.1.1")).await,
            Err(DesktopUpdateError::InstallFailed)
        );
    }
    let temporary_files = std::fs::read_dir(&settings)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect::<Vec<_>>();
    assert_eq!(temporary_files.len(), 1, "失败重试不得累积临时文件");
    assert_eq!(
        temporary_files[0].file_name().to_string_lossy(),
        ".desktop-update-state.tmp"
    );
}

#[tokio::test]
async fn transient_persisted_states_require_a_fresh_check_after_restart() {
    for (label, state) in [
        ("checking", serde_json::json!({"phase": "checking"})),
        (
            "available",
            serde_json::json!({
                "phase": "available",
                "version": "0.1.1",
                "notes": "安全更新",
                "publishedAt": "2026-08-23T00:00:00Z"
            }),
        ),
        (
            "downloading",
            serde_json::json!({"phase": "downloading", "version": "0.1.1"}),
        ),
        (
            "installing",
            serde_json::json!({"phase": "installing", "version": "0.1.1"}),
        ),
    ] {
        let settings = fixture_root(label).join("settings");
        std::fs::create_dir_all(&settings).unwrap();
        let state_path = settings.join("desktop-update-state.json");
        std::fs::write(
            &state_path,
            serde_json::to_vec(&serde_json::json!({
                "schema": 1,
                "revision": 7,
                "state": state
            }))
            .unwrap(),
        )
        .unwrap();

        let controller = DesktopUpdateController::new(settings, current_version());
        let snapshot = controller.snapshot().await;

        assert_eq!(snapshot.revision, 7);
        assert!(matches!(snapshot.state, DesktopUpdateState::Unavailable));
    }
}

#[tokio::test]
async fn truncated_state_is_reported_without_overwriting_the_original_bytes() {
    let settings = fixture_root("truncated").join("settings");
    std::fs::create_dir_all(&settings).unwrap();
    let state_path = settings.join("desktop-update-state.json");
    let truncated = br#"{"schema":1,"revision":9,"state":{"kind":"available""#;
    std::fs::write(&state_path, truncated).unwrap();

    let controller = DesktopUpdateController::new(settings, current_version());
    let snapshot = controller.snapshot().await;

    assert!(matches!(
        snapshot.state,
        DesktopUpdateState::Failed {
            error_kind: DesktopUpdateErrorKind::InvalidMetadata
        }
    ));
    assert_eq!(std::fs::read(state_path).unwrap(), truncated);
}

#[tokio::test]
async fn hardlink_state_target_is_rejected_without_writing_the_linked_file() {
    let settings = fixture_root("hardlink").join("settings");
    std::fs::create_dir_all(&settings).unwrap();
    let controller = DesktopUpdateController::new(settings.clone(), current_version());
    let before = controller.snapshot().await;
    let external = settings.parent().unwrap().join("external.json");
    let original = br#"{"external":"must-not-change"}"#;
    std::fs::write(&external, original).unwrap();
    std::fs::hard_link(&external, settings.join("desktop-update-state.json")).unwrap();

    assert_eq!(
        controller.check(&FakeBackend::available("0.1.1")).await,
        Err(DesktopUpdateError::InstallFailed)
    );
    assert_eq!(controller.snapshot().await, before);
    assert_eq!(std::fs::read(external).unwrap(), original);
}

#[cfg(windows)]
#[tokio::test]
async fn symlink_reparse_state_target_is_rejected_without_writing_the_target() {
    use std::os::windows::fs::{MetadataExt, symlink_file};
    use std::process::Command;

    let settings = fixture_root("symlink").join("settings");
    std::fs::create_dir_all(&settings).unwrap();
    let controller = DesktopUpdateController::new(settings.clone(), current_version());
    let before = controller.snapshot().await;
    let external = settings.parent().unwrap().join("external.json");
    let original = br#"{"external":"must-not-change"}"#;
    std::fs::write(&external, original).unwrap();
    let target = settings.join("desktop-update-state.json");
    if symlink_file(&external, &target).is_err() {
        let status = Command::new("pwsh.exe")
            .args([
                "-NoProfile",
                "-Command",
                "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:DSH_TEST_LINK -Target $env:DSH_TEST_TARGET | Out-Null",
            ])
            .env("DSH_TEST_LINK", &target)
            .env("DSH_TEST_TARGET", settings.parent().unwrap())
            .status()
            .expect("应启动 PowerShell 7 创建 junction 夹具");
        assert!(status.success(), "应创建可验证的 junction reparse 夹具");
    }
    assert_ne!(
        std::fs::symlink_metadata(&target)
            .expect("应读取 reparse 夹具")
            .file_attributes()
            & 0x400,
        0,
        "测试目标必须确实是 reparse point"
    );
    assert_eq!(
        controller.check(&FakeBackend::available("0.1.1")).await,
        Err(DesktopUpdateError::InstallFailed)
    );
    assert_eq!(controller.snapshot().await, before);
    assert_eq!(std::fs::read(external).unwrap(), original);
}
