use dsh_desktop_lib::diagnostics::{
    DiagnosticContext, DiagnosticErrorKind, DiagnosticEvent, DiagnosticLogger, DiagnosticPolicy,
    DiagnosticSink, DiagnosticStage, FileDiagnosticSink, OperationTrace, TraceKind,
};
use dsh_desktop_lib::runtime::RuntimeError;
use dsh_desktop_lib::update::manifest::ManifestVerifier;
use ed25519_dalek::SigningKey;
use semver::Version;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
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
        &OperationTrace::begin(TraceKind::Runtime),
        stage,
        42,
        1,
        Some(43127),
        error_kind,
    )
}

#[derive(Default)]
struct RecordingSink(Mutex<Vec<serde_json::Value>>);

impl DiagnosticSink for RecordingSink {
    fn record(&self, event: DiagnosticEvent) {
        self.0
            .lock()
            .expect("记录锁不应中毒")
            .push(serde_json::to_value(event).expect("事件应可序列化"));
    }
}

fn canonical_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn generated_operation_trace_has_a_fixed_non_secret_format() {
    let event = event(DiagnosticStage::RuntimeStart, None);
    let value = serde_json::to_value(event).expect("事件应可序列化");
    let trace = value["trace_id"].as_str().expect("trace 应为字符串");

    assert!(trace.starts_with("runtime-"));
    assert!(
        trace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    );
    for secret in [
        "sk-proj-AbCdEf1234567890",
        "AKIAIOSFODNN7EXAMPLE",
        "Authorization: Bearer token-value",
        "please summarize my private payroll notes",
        r"C:\\用户\\鲸鱼\\私密配置.yaml",
    ] {
        assert!(!trace.contains(secret));
    }
}

#[test]
fn manifest_production_diagnostics_keep_one_trace_without_serializing_payload() {
    let sink = Arc::new(RecordingSink::default());
    let diagnostics = DiagnosticContext::begin(TraceKind::Update, sink.clone());
    let public_key = SigningKey::from_bytes(&[7; 32]).verifying_key();
    let verifier = ManifestVerifier::new(
        &canonical_hex(public_key.as_bytes()),
        Version::parse("0.1.0").expect("固定版本合法"),
    )
    .expect("测试公钥合法");
    let payload = br#"{"prompt":"private ASCII body","api_key":"sk-proj-AbCdEf123"}"#;

    assert!(
        verifier
            .verify_with_context(payload, "AKIAIOSFODNN7EXAMPLE", &diagnostics)
            .is_err()
    );

    let events = sink.0.lock().expect("记录锁不应中毒");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["trace_id"], events[1]["trace_id"]);
    let serialized = serde_json::to_string(&*events).expect("记录应可序列化");
    for secret in [
        "private ASCII body",
        "sk-proj-AbCdEf123",
        "AKIAIOSFODNN7EXAMPLE",
    ] {
        assert!(!serialized.contains(secret));
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
            &OperationTrace::begin(TraceKind::Update),
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
async fn preexisting_oversized_truncated_jsonl_is_safely_reclaimed_in_place() {
    let directory = isolated_directory("oversized-recovery");
    fs::create_dir_all(&directory).expect("应创建日志目录");
    let oversized = directory.join("diagnostics-1.jsonl");
    fs::write(&oversized, vec![b'{'; 512]).expect("应预置截断的超限 JSONL");
    let policy = DiagnosticPolicy {
        max_file_bytes: 180,
        slot_count: 3,
    };
    let logger = DiagnosticLogger::new(directory.clone(), policy).expect("策略合法");

    logger
        .write(&event(DiagnosticStage::UpdateProbe, None))
        .await
        .expect("应在同一句柄安全收敛超限槽位");

    for slot in 0..policy.slot_count {
        let path = directory.join(format!("diagnostics-{slot}.jsonl"));
        if path.exists() {
            assert!(fs::metadata(path).expect("应读取槽位").len() <= policy.max_file_bytes);
        }
    }
    assert_eq!(fs::metadata(oversized).expect("超限槽位仍应存在").len(), 0);
}

#[tokio::test]
async fn under_limit_partial_jsonl_is_truncated_to_its_last_valid_line() {
    let directory = isolated_directory("partial-recovery");
    fs::create_dir_all(&directory).expect("应创建日志目录");
    let slot = directory.join("diagnostics-0.jsonl");
    let valid = serde_json::to_vec(&event(DiagnosticStage::OfficialCheck, None))
        .expect("合法事件应可序列化");
    let mut fixture = valid.clone();
    fixture.push(b'\n');
    fixture.extend_from_slice(br#"{"trace_id":"sk-proj-partial""#);
    fs::write(&slot, fixture).expect("应预置小于上限的半行");
    let logger = DiagnosticLogger::new(
        directory,
        DiagnosticPolicy {
            max_file_bytes: 1024,
            slot_count: 2,
        },
    )
    .expect("策略合法");

    logger
        .write(&event(DiagnosticStage::CompatibilityCheck, None))
        .await
        .expect("应保留合法前缀并修复半行");

    let repaired = fs::read(&slot).expect("应读取修复后的槽位");
    assert!(repaired.starts_with(&[valid.as_slice(), b"\n"].concat()));
    assert!(!String::from_utf8_lossy(&repaired).contains("sk-proj-partial"));
    for line in repaired
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        serde_json::from_slice::<serde_json::Value>(line).expect("每行必须是完整 JSON");
    }
}

#[tokio::test]
async fn complete_json_with_a_non_generated_secret_trace_is_not_preserved() {
    let directory = isolated_directory("secret-trace-recovery");
    fs::create_dir_all(&directory).expect("应创建日志目录");
    let slot = directory.join("diagnostics-0.jsonl");
    let mut forged = serde_json::to_value(event(DiagnosticStage::OfficialCheck, None))
        .expect("合法事件应可序列化");
    forged["trace_id"] = serde_json::Value::String("update-sk-proj-AbCdEf123".to_owned());
    let mut bytes = serde_json::to_vec(&forged).expect("伪造夹具应可序列化");
    bytes.push(b'\n');
    fs::write(&slot, bytes).expect("应预置完整但非生成 trace 的 JSONL");
    let logger = DiagnosticLogger::new(
        directory,
        DiagnosticPolicy {
            max_file_bytes: 1024,
            slot_count: 2,
        },
    )
    .expect("策略合法");

    logger
        .write(&event(DiagnosticStage::CompatibilityCheck, None))
        .await
        .expect("应拒绝保留非生成 trace");

    let repaired = fs::read_to_string(slot).expect("应读取修复后的槽位");
    assert!(!repaired.contains("sk-proj-AbCdEf123"));
    for line in repaired.lines() {
        serde_json::from_str::<serde_json::Value>(line).expect("每行必须是完整 JSON");
    }
}

#[tokio::test]
async fn recovery_rejects_unknown_keys_types_and_enum_values_without_echoing_secrets() {
    for (index, mutate) in [
        ("stage", "private ASCII body"),
        ("error_kind", "AKIAIOSFODNN7EXAMPLE"),
        ("unknown", "Authorization: Bearer sk-proj-secret"),
        ("elapsed_ms", r"C:\用户\私密.json"),
    ]
    .into_iter()
    .enumerate()
    {
        let directory = isolated_directory(&format!("invalid-schema-{index}"));
        fs::create_dir_all(&directory).expect("应创建日志目录");
        let slot = directory.join("diagnostics-0.jsonl");
        let mut forged = serde_json::to_value(event(DiagnosticStage::OfficialCheck, None))
            .expect("合法事件应可序列化");
        forged[mutate.0] = serde_json::Value::String(mutate.1.to_owned());
        let mut bytes = serde_json::to_vec(&forged).expect("伪造夹具应可序列化");
        bytes.push(b'\n');
        fs::write(&slot, bytes).expect("应预置不合法 JSONL");
        let logger = DiagnosticLogger::new(
            directory,
            DiagnosticPolicy {
                max_file_bytes: 1024,
                slot_count: 2,
            },
        )
        .expect("策略合法");

        logger
            .write(&event(DiagnosticStage::CompatibilityCheck, None))
            .await
            .expect("应拒绝不合法恢复记录");

        let repaired = fs::read_to_string(slot).expect("应读取修复后的槽位");
        assert!(!repaired.contains(mutate.1));
        for line in repaired.lines() {
            serde_json::from_str::<serde_json::Value>(line).expect("每行必须是完整 JSON");
        }
    }
}

#[tokio::test]
async fn no_fail_sink_absorbs_write_failures_and_queue_pressure() {
    let occupied_path = isolated_directory("sink-failure");
    fs::write(&occupied_path, b"not a directory").expect("应创建占位文件");
    let logger =
        DiagnosticLogger::new(occupied_path, DiagnosticPolicy::default()).expect("策略本身合法");
    let sink = FileDiagnosticSink::new(logger, 1).expect("队列容量合法");

    for _ in 0..100 {
        sink.record(event(DiagnosticStage::RuntimeStart, None));
    }

    sink.flush().await;
}

#[tokio::test]
async fn closed_worker_makes_later_records_a_noop_without_panicking() {
    let directory = isolated_directory("worker-close");
    let logger = DiagnosticLogger::new(directory.clone(), DiagnosticPolicy::default())
        .expect("策略本身合法");
    let sink = FileDiagnosticSink::new(logger, 4).expect("队列容量合法");

    sink.shutdown().await;
    sink.record(event(DiagnosticStage::RuntimeStart, None));

    assert!(!directory.join("diagnostics-0.jsonl").exists());
}

#[tokio::test]
async fn competing_loggers_never_leave_partial_json_or_exceed_slot_limit() {
    let directory = isolated_directory("competing-loggers");
    let policy = DiagnosticPolicy {
        max_file_bytes: 1024,
        slot_count: 2,
    };
    let first = DiagnosticLogger::new(directory.clone(), policy).expect("策略合法");
    let second = DiagnosticLogger::new(directory.clone(), policy).expect("策略合法");
    let first_event = event(DiagnosticStage::OfficialCheck, None);
    let second_event = event(DiagnosticStage::CompatibilityCheck, None);

    let (first_result, second_result) =
        tokio::join!(first.write(&first_event), second.write(&second_event),);
    assert!(first_result.is_ok() || second_result.is_ok());

    for slot in 0..policy.slot_count {
        let path = directory.join(format!("diagnostics-{slot}.jsonl"));
        if !path.exists() {
            continue;
        }
        let bytes = fs::read_to_string(&path).expect("槽位应为 UTF-8 JSONL");
        for line in bytes.lines() {
            serde_json::from_str::<serde_json::Value>(line).expect("不得留下部分 JSON");
        }
        assert!(fs::metadata(path).expect("应读取槽位").len() <= policy.max_file_bytes);
    }
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

#[cfg(windows)]
struct RestoredWindowsDacl {
    path: Vec<u16>,
    original_dacl: *mut windows::Win32::Security::ACL,
    original_descriptor: windows::Win32::Security::PSECURITY_DESCRIPTOR,
    added_dacl: *mut windows::Win32::Security::ACL,
}

#[cfg(windows)]
impl Drop for RestoredWindowsDacl {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
        use windows::Win32::Security::DACL_SECURITY_INFORMATION;
        use windows::core::PWSTR;

        unsafe {
            let _ = SetNamedSecurityInfoW(
                PWSTR(self.path.as_mut_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(self.original_dacl),
                None,
            );
            let _ = LocalFree(Some(HLOCAL(self.added_dacl.cast())));
            let _ = LocalFree(Some(HLOCAL(self.original_descriptor.0)));
        }
    }
}

#[cfg(windows)]
fn grant_everyone_write(path: &std::path::Path) -> RestoredWindowsDacl {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{ERROR_SUCCESS, GENERIC_WRITE};
    use windows::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, SE_FILE_OBJECT, SetEntriesInAclW,
        SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
    };
    use windows::Win32::Security::{
        ACL, CreateWellKnownSid, DACL_SECURITY_INFORMATION, NO_INHERITANCE, PSECURITY_DESCRIPTOR,
        PSID, WinWorldSid,
    };
    use windows::core::{PCWSTR, PWSTR};

    let mut wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut original_dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut original_dacl),
            None,
            &mut descriptor,
        )
    };
    assert_eq!(status, ERROR_SUCCESS, "应能读取测试槽位 DACL");
    assert!(!original_dacl.is_null(), "测试槽位不得使用 null DACL");

    let mut sid_size = 0_u32;
    let _ = unsafe { CreateWellKnownSid(WinWorldSid, None, None, &mut sid_size) };
    assert!(sid_size > 0, "应能查询 Everyone SID 大小");
    let mut sid = vec![0_u8; sid_size as usize];
    let everyone = PSID(sid.as_mut_ptr().cast());
    unsafe { CreateWellKnownSid(WinWorldSid, None, Some(everyone), &mut sid_size) }
        .expect("应能创建 Everyone SID");
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: GENERIC_WRITE.0,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: Default::default(),
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
            ptstrName: PWSTR(everyone.0.cast()),
        },
    };
    let mut added_dacl: *mut ACL = std::ptr::null_mut();
    let status = unsafe { SetEntriesInAclW(Some(&[entry]), Some(original_dacl), &mut added_dacl) };
    assert_eq!(status, ERROR_SUCCESS, "应能构造宽泛测试 DACL");
    let status = unsafe {
        SetNamedSecurityInfoW(
            PWSTR(wide.as_mut_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(added_dacl),
            None,
        )
    };
    assert_eq!(status, ERROR_SUCCESS, "应能设置宽泛测试 DACL");
    RestoredWindowsDacl {
        path: wide,
        original_dacl,
        original_descriptor: descriptor,
        added_dacl,
    }
}

#[cfg(windows)]
#[tokio::test]
async fn everyone_write_ace_is_rejected_and_original_dacl_is_restored() {
    let directory = isolated_directory("broad-dacl");
    fs::create_dir_all(&directory).expect("应创建日志目录");
    let slot = directory.join("diagnostics-0.jsonl");
    fs::write(&slot, b"").expect("应创建固定槽位");
    let _restore = grant_everyone_write(&slot);
    let logger = DiagnosticLogger::new(directory, DiagnosticPolicy::default()).expect("策略合法");

    let result = logger
        .write(&event(DiagnosticStage::RuntimeStart, None))
        .await;

    assert!(result.is_err(), "Everyone 写 ACE 必须被生产检查拒绝");
}

#[cfg(windows)]
#[tokio::test]
async fn reparse_log_directory_is_rejected_when_windows_allows_fixture_creation() {
    use std::os::windows::fs::symlink_dir;

    let target = isolated_directory("reparse-target");
    let link = isolated_directory("reparse-link");
    fs::create_dir_all(&target).expect("应创建真实目标目录");
    if symlink_dir(&target, &link).is_err() {
        return;
    }
    let logger = DiagnosticLogger::new(link, DiagnosticPolicy::default()).expect("策略合法");

    assert!(
        logger
            .write(&event(DiagnosticStage::RuntimeStart, None))
            .await
            .is_err()
    );
}
