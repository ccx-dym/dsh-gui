use dsh_desktop_lib::diagnostics::{
    DiagnosticErrorKind, DiagnosticEvent, DiagnosticLogger, DiagnosticPolicy, DiagnosticSink,
    DiagnosticStage, DiagnosticTraceId,
};
use dsh_desktop_lib::runtime::RuntimeError;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn isolated_directory(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间应晚于 Unix epoch")
        .as_nanos();
    #[cfg(windows)]
    let root = PathBuf::from(std::env::var_os("APPDATA").expect("Windows 测试应有 APPDATA"));
    #[cfg(not(windows))]
    let root = std::env::temp_dir();
    root.join(format!(
        "dsh-desktop-diagnostics-{label}-{}-{nonce}",
        std::process::id()
    ))
}

fn event(stage: DiagnosticStage, error_kind: Option<DiagnosticErrorKind>) -> DiagnosticEvent {
    DiagnosticEvent::new(
        DiagnosticTraceId::parse("trace-001").expect("固定 trace 合法"),
        stage,
        42,
        1,
        Some(43127),
        error_kind,
    )
}

#[test]
fn diagnostic_metadata_rejects_secrets_user_text_queries_and_unicode_paths() {
    for unsafe_value in [
        "sk-secret-123456",
        "Authorization: Bearer token-value",
        "https://example.invalid/update?api_key=secret",
        "请帮我总结这段私人正文",
        r"C:\\用户\\鲸鱼\\私密配置.yaml",
    ] {
        assert!(
            DiagnosticTraceId::parse(unsafe_value).is_err(),
            "不安全输入不得成为诊断字段: {unsafe_value}"
        );
    }
}

#[test]
fn runtime_user_message_never_echoes_dynamic_error_text() {
    let unsafe_errors = [
        RuntimeError::Tauri("Authorization: Bearer token-value".to_owned()),
        RuntimeError::InvalidUrl("https://example.invalid/?api_key=secret".to_owned()),
        RuntimeError::Io(std::io::Error::other(
            r"C:\用户\鲸鱼\私密配置.yaml: sk-secret-123456",
        )),
    ];

    for error in unsafe_errors {
        let message = error.safe_user_message();
        assert_eq!(message, "DSH 运行时操作失败，请稍后重试");
        assert!(!message.contains("secret"));
        assert!(!message.contains("Authorization"));
        assert!(!message.contains("用户"));
    }
}

#[test]
fn policy_rejects_unbounded_file_and_slot_limits() {
    let directory = isolated_directory("invalid-policy");
    assert!(
        DiagnosticLogger::new(
            directory.clone(),
            DiagnosticPolicy {
                max_file_bytes: 4 * 1024 * 1024 + 1,
                slot_count: 3,
            },
        )
        .is_err()
    );
    assert!(
        DiagnosticLogger::new(
            directory,
            DiagnosticPolicy {
                max_file_bytes: 1024,
                slot_count: 17,
            },
        )
        .is_err()
    );
}

#[tokio::test]
async fn serialized_event_contains_only_the_fixed_safe_schema() {
    let directory = isolated_directory("schema");
    let logger = DiagnosticLogger::new(
        directory.clone(),
        DiagnosticPolicy {
            max_file_bytes: 1024,
            slot_count: 2,
        },
    )
    .expect("策略合法");
    logger
        .write(&event(
            DiagnosticStage::RuntimeStart,
            Some(DiagnosticErrorKind::IoError),
        ))
        .await
        .expect("应写入日志");

    let line = fs::read_to_string(directory.join("diagnostics-0.jsonl"))
        .expect("应读取第一个固定日志槽位");
    let value: serde_json::Value = serde_json::from_str(line.trim()).expect("应为 JSONL");
    let keys = value
        .as_object()
        .expect("事件应为对象")
        .keys()
        .cloned()
        .collect::<Vec<_>>();

    assert_eq!(
        keys,
        vec![
            "elapsed_ms",
            "error_kind",
            "pid",
            "retry",
            "stage",
            "trace_id"
        ]
    );
    assert_eq!(value["stage"], "runtime_start");
}

#[tokio::test]
async fn logger_rotates_within_fixed_slots_and_keeps_every_file_bounded() {
    let directory = isolated_directory("bounded");
    let policy = DiagnosticPolicy {
        max_file_bytes: 180,
        slot_count: 2,
    };
    let logger = DiagnosticLogger::new(directory.clone(), policy).expect("策略合法");

    for retry in 0..20 {
        let current = DiagnosticEvent::new(
            DiagnosticTraceId::parse("trace-001").expect("固定 trace 合法"),
            DiagnosticStage::UpdateProbe,
            42,
            retry,
            Some(43127),
            None,
        );
        logger.write(&current).await.expect("滚动写入应成功");
    }

    let entries = fs::read_dir(&directory)
        .expect("应读取日志目录")
        .collect::<Result<Vec<_>, _>>()
        .expect("目录项应可读");
    assert_eq!(entries.len(), 2, "不得创建无界历史文件");
    for entry in entries {
        assert!(
            entry.metadata().expect("应读取日志元数据").len() <= policy.max_file_bytes,
            "每个固定槽位必须受大小上限约束"
        );
    }
}

#[tokio::test]
async fn no_fail_sink_absorbs_write_failures_and_queue_pressure() {
    let occupied_path = isolated_directory("sink-failure");
    fs::write(&occupied_path, b"not a directory").expect("应创建占位文件");
    let logger =
        DiagnosticLogger::new(occupied_path, DiagnosticPolicy::default()).expect("策略本身合法");
    let sink = DiagnosticSink::new(logger, 1).expect("队列容量合法");

    for _ in 0..100 {
        sink.record(event(DiagnosticStage::RuntimeStart, None));
    }

    sink.flush().await;
}

#[tokio::test]
async fn write_failure_is_returned_without_panicking() {
    let occupied_path = isolated_directory("write-failure");
    fs::write(&occupied_path, b"not a directory").expect("应创建占位文件");
    let logger =
        DiagnosticLogger::new(occupied_path, DiagnosticPolicy::default()).expect("策略本身合法");

    let result = logger.write(&event(DiagnosticStage::TrayOpen, None)).await;

    assert!(result.is_err());
}

#[cfg(windows)]
#[tokio::test]
async fn existing_hardlinked_slot_is_rejected_without_modifying_its_content() {
    let directory = isolated_directory("hardlink");
    fs::create_dir_all(&directory).expect("应创建日志目录");
    let slot = directory.join("diagnostics-0.jsonl");
    let alias = directory.join("unexpected-alias.jsonl");
    fs::write(&slot, b"trusted previous log\n").expect("应创建已有槽位");
    fs::hard_link(&slot, &alias).expect("测试卷应支持硬链接");
    let logger =
        DiagnosticLogger::new(directory, DiagnosticPolicy::default()).expect("策略本身合法");

    let result = logger
        .write(&event(DiagnosticStage::RuntimeStart, None))
        .await;

    assert!(result.is_err());
    assert_eq!(
        fs::read(&slot).expect("应读取原槽位"),
        b"trusted previous log\n"
    );
}
