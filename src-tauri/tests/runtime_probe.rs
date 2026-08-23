#![cfg(windows)]

use dsh_desktop_lib::app_controller::ProbeLease;
use dsh_desktop_lib::diagnostics::{DiagnosticContext, TraceKind};
use dsh_desktop_lib::paths::{AppPaths, RuntimeLayout};
use dsh_desktop_lib::runtime::command::RuntimeLaunchSpec;
use dsh_desktop_lib::runtime::health::ReadyProbe;
use dsh_desktop_lib::runtime::install_state::{
    ActiveDeployment, DataGeneration, InstallStateStore, InstalledRuntime,
};
use dsh_desktop_lib::runtime::process::StopOutcome;
use dsh_desktop_lib::runtime::{ProcessLauncher, ReadinessSignal, RuntimeError, RuntimeProcess};
use dsh_desktop_lib::update::probe::{
    ProbeCancellation, ProbeErrorKind, ProbePermissionInspector, ProbePhase, ProbePolicy,
    ProbeStorageInspector, ProbeWorkspace, RuntimeProbe, read_passed_generation_state,
};
use semver::Version;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy)]
enum ProcessMode {
    Ready,
    NoStdout,
    ExitEarly,
    StopFails,
}

fn diagnostics() -> DiagnosticContext {
    DiagnosticContext::noop(TraceKind::Update)
}

struct FakeProcess {
    mode: ProcessMode,
    stops: Arc<AtomicUsize>,
}

impl RuntimeProcess for FakeProcess {
    fn id(&self) -> u32 {
        42
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, RuntimeError> {
        if matches!(self.mode, ProcessMode::ExitEarly) {
            use std::os::windows::process::ExitStatusExt;
            Ok(Some(ExitStatus::from_raw(1)))
        } else {
            Ok(None)
        }
    }

    fn stop(&mut self, _grace: Duration) -> Result<StopOutcome, RuntimeError> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, ProcessMode::StopFails) {
            Err(RuntimeError::Tauri("sensitive cleanup detail".to_owned()))
        } else {
            Ok(StopOutcome::Terminated)
        }
    }

    fn wait_for_readiness(
        &mut self,
        port: u16,
        timeout: Duration,
    ) -> Result<ReadinessSignal, RuntimeError> {
        if matches!(self.mode, ProcessMode::Ready | ProcessMode::StopFails) {
            Ok(ReadinessSignal::Web { port })
        } else {
            std::thread::sleep(timeout.min(Duration::from_millis(10)));
            Err(RuntimeError::OutputReadinessTimeout {
                port,
                timeout_ms: timeout.as_millis() as u64,
            })
        }
    }
}

struct FakeLauncher {
    mode: ProcessMode,
    stops: Arc<AtomicUsize>,
    specs: Arc<Mutex<Vec<RuntimeLaunchSpec>>>,
}

enum RuntimeMutation {
    Rewrite(PathBuf),
    Add(PathBuf),
}

struct MutatingLauncher {
    mutation: RuntimeMutation,
    stops: Arc<AtomicUsize>,
}

impl ProcessLauncher for MutatingLauncher {
    fn spawn(&self, _spec: &RuntimeLaunchSpec) -> Result<Box<dyn RuntimeProcess>, RuntimeError> {
        match &self.mutation {
            RuntimeMutation::Rewrite(path) => fs::write(path, b"dependency-v2")?,
            RuntimeMutation::Add(path) => fs::write(path, b"unexpected")?,
        }
        Ok(Box::new(FakeProcess {
            mode: ProcessMode::Ready,
            stops: Arc::clone(&self.stops),
        }))
    }
}

impl ProcessLauncher for FakeLauncher {
    fn spawn(&self, spec: &RuntimeLaunchSpec) -> Result<Box<dyn RuntimeProcess>, RuntimeError> {
        self.specs.lock().expect("spec lock").push(spec.clone());
        Ok(Box::new(FakeProcess {
            mode: self.mode,
            stops: Arc::clone(&self.stops),
        }))
    }
}

struct FakeHealth {
    ready: bool,
}

impl ReadyProbe for FakeHealth {
    fn wait_until_ready(&self, port: u16, _timeout: Duration) -> Result<String, RuntimeError> {
        if self.ready {
            Ok(format!("http://127.0.0.1:{port}"))
        } else {
            Err(RuntimeError::HealthTimeout {
                port,
                timeout_ms: 10,
            })
        }
    }
}

struct PlentyOfSpace;

impl ProbeStorageInspector for PlentyOfSpace {
    fn available_bytes(&self, _path: &Path) -> Result<u64, std::io::Error> {
        Ok(u64::MAX)
    }
}

struct SafePermissions;

impl ProbePermissionInspector for SafePermissions {
    fn ensure_private(&self, _path: &Path) -> Result<(), std::io::Error> {
        Ok(())
    }
}

fn test_probe_with_dependencies(
    policy: ProbePolicy,
    launcher: Arc<dyn ProcessLauncher>,
    health: Arc<dyn ReadyProbe>,
    storage: Arc<dyn ProbeStorageInspector>,
) -> Result<RuntimeProbe, dsh_desktop_lib::update::probe::ProbeError> {
    RuntimeProbe::with_inspectors(policy, launcher, health, storage, Arc::new(SafePermissions))
}

struct NoSpace;

impl ProbeStorageInspector for NoSpace {
    fn available_bytes(&self, _path: &Path) -> Result<u64, std::io::Error> {
        std::thread::sleep(Duration::from_millis(15));
        Ok(0)
    }
}

struct SlowStorage;

impl ProbeStorageInspector for SlowStorage {
    fn available_bytes(&self, _path: &Path) -> Result<u64, std::io::Error> {
        std::thread::sleep(Duration::from_millis(250));
        Ok(u64::MAX)
    }
}

struct Fixture {
    layout: RuntimeLayout,
    runtime: InstalledRuntime,
    candidate: DataGeneration,
    active: DataGeneration,
    workspace: PathBuf,
    candidate_dir: PathBuf,
    active_dir: PathBuf,
    runtime_dependency: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dsh-runtime-probe-{label}-{unique}"));
        let paths = AppPaths::from_roots(&root.join("roaming"), &root.join("local"));
        let layout = RuntimeLayout::from_paths(&paths);
        let runtime = InstalledRuntime::with_node_version("0.1.1-rc.1", "a".repeat(64), "24.15.0")
            .expect("runtime");
        let candidate = DataGeneration::new("candidate-001").expect("candidate");
        let active = DataGeneration::new("active-001").expect("active");
        let runtime_dir = layout.runtime_dir(&runtime);
        let node_dir = runtime_dir.join("node-v24.15.0-win-x64");
        let cli_dir = runtime_dir.join("app/node_modules/@deepseek-ai/dsh/lib");
        fs::create_dir_all(&node_dir).expect("node dir");
        fs::create_dir_all(&cli_dir).expect("cli dir");
        fs::write(node_dir.join("node.exe"), b"node").expect("node");
        fs::write(cli_dir.join("bin.js"), b"export {}").expect("cli");
        let dependency = runtime_dir.join("app/node_modules/runtime-dependency/data.bin");
        fs::create_dir_all(dependency.parent().expect("dependency parent"))
            .expect("dependency dir");
        fs::write(&dependency, b"dependency-v1").expect("dependency");
        let inventory_payload = [
            ("node-v24.15.0-win-x64/node.exe", b"node".as_slice()),
            (
                "app/node_modules/@deepseek-ai/dsh/lib/bin.js",
                b"export {}".as_slice(),
            ),
            (
                "app/node_modules/runtime-dependency/data.bin",
                b"dependency-v1".as_slice(),
            ),
        ];
        let inventory = inventory_payload
            .iter()
            .map(|(path, bytes)| {
                serde_json::json!({
                    "path": path,
                    "size": bytes.len(),
                    "sha256": format!("{:x}", Sha256::digest(bytes)),
                })
            })
            .collect::<Vec<_>>();
        fs::write(
            runtime_dir.join("inventory.json"),
            serde_json::to_vec(&inventory).expect("inventory json"),
        )
        .expect("inventory");
        let candidate_dir = layout.generation_dir(&candidate);
        let active_dir = layout.generation_dir(&active);
        let workspace = root.join("workspace");
        fs::create_dir_all(&candidate_dir).expect("candidate dir");
        fs::create_dir_all(&active_dir).expect("active dir");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(active_dir.join("secret.txt"), b"unchanged").expect("active sentinel");
        Self {
            layout,
            runtime,
            candidate,
            active,
            workspace,
            candidate_dir,
            active_dir,
            runtime_dependency: dependency,
        }
    }

    fn workspace(&self) -> ProbeWorkspace {
        InstallStateStore::new(self.layout.clone())
            .save(&ActiveDeployment::with_project_workspace(
                self.runtime.clone(),
                self.active.clone(),
                "2026-08-22T00:00:00Z".to_owned(),
                self.workspace.clone(),
            ))
            .expect("active deployment");
        ProbeWorkspace::new(
            self.layout.clone(),
            self.runtime.clone(),
            Version::parse("24.15.0").expect("node version"),
            self.candidate.clone(),
            self.workspace.clone(),
            &ProbeLease::for_test(),
        )
        .expect("probe workspace")
    }

    fn fresh_workspace(&self) -> ProbeWorkspace {
        ProbeWorkspace::new(
            self.layout.clone(),
            self.runtime.clone(),
            Version::parse("24.15.0").expect("node version"),
            self.candidate.clone(),
            self.workspace.clone(),
            &ProbeLease::for_test(),
        )
        .expect("fresh probe workspace")
    }
}

fn probe(
    mode: ProcessMode,
    http_ready: bool,
) -> (
    RuntimeProbe,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<RuntimeLaunchSpec>>>,
) {
    let stops = Arc::new(AtomicUsize::new(0));
    let specs = Arc::new(Mutex::new(Vec::new()));
    let probe = test_probe_with_dependencies(
        ProbePolicy {
            timeout: Duration::from_millis(500),
            poll_interval: Duration::from_millis(10),
            stop_grace: Duration::ZERO,
            max_files: 64,
            max_candidate_bytes: 1024 * 1024,
            required_free_bytes: 1,
        },
        Arc::new(FakeLauncher {
            mode,
            stops: Arc::clone(&stops),
            specs: Arc::clone(&specs),
        }),
        Arc::new(FakeHealth { ready: http_ready }),
        Arc::new(PlentyOfSpace),
    )
    .expect("probe policy");
    (probe, stops, specs)
}

#[tokio::test]
async fn empty_candidate_requires_both_readiness_gates_and_is_reclaimed() {
    let fixture = Fixture::new("success");
    let (probe, stops, specs) = probe(ProcessMode::Ready, true);

    let report = probe
        .probe(
            fixture.fresh_workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("probe execution");

    assert_eq!(report.phase, ProbePhase::Passed);
    assert_eq!(report.error_kind, None);
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read(fixture.active_dir.join("secret.txt")).expect("active"),
        b"unchanged"
    );
    let spec = &specs.lock().expect("spec lock")[0];
    assert_eq!(
        spec.env.get("DSH_HOME"),
        Some(&fixture.candidate_dir.display().to_string())
    );
    assert!(
        fixture
            .layout
            .generation_root()
            .join(".state")
            .join(&fixture.candidate.id)
            .join("passed.json")
            .is_file()
    );
    let serialized = serde_json::to_value(&report).expect("safe report json");
    let keys = serialized
        .as_object()
        .expect("report object")
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        keys,
        std::collections::BTreeSet::from([
            "elapsed_ms",
            "error_kind",
            "phase",
            "retry_count",
            "trace_id",
            "version",
        ])
    );
    let text = serialized.to_string();
    assert!(!text.contains("http://"));
    assert!(!text.contains("secret.txt"));
}

#[tokio::test]
async fn project_workspace_is_canonicalized_before_launch() {
    let fixture = Fixture::new("canonical-workspace");
    let workspace = ProbeWorkspace::new(
        fixture.layout.clone(),
        fixture.runtime.clone(),
        Version::parse("24.15.0").expect("node version"),
        fixture.candidate.clone(),
        fixture.workspace.join("."),
        &ProbeLease::for_test(),
    )
    .expect("workspace");
    let (probe, _, specs) = probe(ProcessMode::Ready, true);

    let report = probe
        .probe(workspace, ProbeCancellation::new(), &diagnostics())
        .await
        .expect("probe");

    assert_eq!(report.phase, ProbePhase::Passed);
    assert_eq!(
        specs.lock().expect("specs")[0].cwd,
        fixture.workspace.canonicalize().expect("canonical")
    );
}

#[tokio::test]
async fn runtime_dependency_modified_during_probe_cannot_pass_inventory_recheck() {
    let fixture = Fixture::new("runtime-mutated");
    let stops = Arc::new(AtomicUsize::new(0));
    let probe = RuntimeProbe::with_inspectors(
        ProbePolicy {
            // MutatingLauncher::spawn 是明确同步点：启动前 inventory 已校验完成，
            // 写入后必须为停止后的完整复检预留足够预算，避免 Windows 调度抖动
            // 把安全断言误归类成 readiness timeout。
            timeout: Duration::from_secs(2),
            poll_interval: Duration::from_millis(10),
            stop_grace: Duration::ZERO,
            max_files: 64,
            max_candidate_bytes: 1024 * 1024,
            required_free_bytes: 1,
        },
        Arc::new(MutatingLauncher {
            mutation: RuntimeMutation::Rewrite(fixture.runtime_dependency.clone()),
            stops: Arc::clone(&stops),
        }),
        Arc::new(FakeHealth { ready: true }),
        Arc::new(PlentyOfSpace),
        Arc::new(SafePermissions),
    )
    .expect("probe");

    let report = probe
        .probe(
            fixture.fresh_workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("report");

    assert_eq!(report.phase, ProbePhase::Failed);
    assert_eq!(
        report.error_kind,
        Some(ProbeErrorKind::RuntimeIntegrityFailed)
    );
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn runtime_extra_file_created_during_probe_cannot_pass_inventory_recheck() {
    let fixture = Fixture::new("runtime-extra");
    let stops = Arc::new(AtomicUsize::new(0));
    let extra = fixture
        .runtime_dependency
        .parent()
        .expect("parent")
        .join("extra.bin");
    let probe = RuntimeProbe::with_inspectors(
        ProbePolicy {
            // 与依赖改写场景相同，额外文件在 spawn 同步写入；这里验证的是
            // post-check 的闭包检测，不应让极短测试预算主导失败类别。
            timeout: Duration::from_secs(2),
            poll_interval: Duration::from_millis(10),
            stop_grace: Duration::ZERO,
            max_files: 64,
            max_candidate_bytes: 1024 * 1024,
            required_free_bytes: 1,
        },
        Arc::new(MutatingLauncher {
            mutation: RuntimeMutation::Add(extra),
            stops: Arc::clone(&stops),
        }),
        Arc::new(FakeHealth { ready: true }),
        Arc::new(PlentyOfSpace),
        Arc::new(SafePermissions),
    )
    .expect("probe");

    let report = probe
        .probe(
            fixture.fresh_workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("report");

    assert_eq!(report.phase, ProbePhase::Failed);
    assert_eq!(
        report.error_kind,
        Some(ProbeErrorKind::RuntimeIntegrityFailed)
    );
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stdout_without_http_is_invalid_webui_and_reclaims_process_tree() {
    let fixture = Fixture::new("invalid-webui");
    let (probe, stops, _) = probe(ProcessMode::Ready, false);

    let report = probe
        .probe(
            fixture.workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("probe execution");

    assert_eq!(report.phase, ProbePhase::Failed);
    assert_eq!(report.error_kind, Some(ProbeErrorKind::InvalidWebUi));
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn missing_stdout_times_out_even_when_http_is_ready() {
    let fixture = Fixture::new("missing-stdout");
    let (probe, stops, _) = probe(ProcessMode::NoStdout, true);

    let report = probe
        .probe(
            fixture.workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("probe execution");

    assert_eq!(report.error_kind, Some(ProbeErrorKind::ReadinessTimeout));
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn early_nonzero_exit_is_typed_and_reclaimed() {
    let fixture = Fixture::new("early-exit");
    let (probe, stops, _) = probe(ProcessMode::ExitEarly, true);

    let report = probe
        .probe(
            fixture.workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("probe execution");

    assert_eq!(report.error_kind, Some(ProbeErrorKind::ProcessExitedEarly));
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_interrupts_wait_and_reclaims_process_tree() {
    let fixture = Fixture::new("cancel");
    let (probe, stops, _) = probe(ProcessMode::NoStdout, false);
    let cancellation = ProbeCancellation::new();
    let cancelling = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancelling.cancel();
    });

    let report = probe
        .probe(fixture.workspace(), cancellation, &diagnostics())
        .await
        .expect("probe execution");

    assert_eq!(report.error_kind, Some(ProbeErrorKind::Cancelled));
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cleanup_failure_overrides_readiness_success_without_leaking_detail() {
    let fixture = Fixture::new("cleanup-failure");
    let (probe, stops, _) = probe(ProcessMode::StopFails, true);

    let report = probe
        .probe(
            fixture.workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("cleanup failure is reported");

    assert_eq!(report.error_kind, Some(ProbeErrorKind::CleanupFailed));
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert!(
        !serde_json::to_string(&report)
            .expect("report json")
            .contains("sensitive cleanup detail")
    );
}

#[tokio::test]
async fn insufficient_space_is_a_failed_report_before_any_process_starts() {
    let fixture = Fixture::new("no-space");
    let stops = Arc::new(AtomicUsize::new(0));
    let specs = Arc::new(Mutex::new(Vec::new()));
    let probe = test_probe_with_dependencies(
        ProbePolicy {
            timeout: Duration::from_millis(500),
            poll_interval: Duration::from_millis(10),
            stop_grace: Duration::ZERO,
            max_files: 64,
            max_candidate_bytes: 1024,
            required_free_bytes: 1,
        },
        Arc::new(FakeLauncher {
            mode: ProcessMode::Ready,
            stops,
            specs: Arc::clone(&specs),
        }),
        Arc::new(FakeHealth { ready: true }),
        Arc::new(NoSpace),
    )
    .expect("policy");

    let report = probe
        .probe(
            fixture.workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("preflight is reported");

    assert_eq!(report.phase, ProbePhase::Failed);
    assert_eq!(report.error_kind, Some(ProbeErrorKind::CandidateRejected));
    assert!(report.elapsed_ms >= 10);
    assert!(specs.lock().expect("spec lock").is_empty());
    assert!(
        fixture
            .layout
            .generation_root()
            .join(".state")
            .join(&fixture.candidate.id)
            .join("failed.json")
            .is_file()
    );
}

#[tokio::test]
async fn cancellation_during_preflight_is_reported_without_launching() {
    let fixture = Fixture::new("cancel-preflight");
    let specs = Arc::new(Mutex::new(Vec::new()));
    let probe = test_probe_with_dependencies(
        ProbePolicy {
            timeout: Duration::from_millis(200),
            poll_interval: Duration::from_millis(10),
            stop_grace: Duration::ZERO,
            max_files: 64,
            max_candidate_bytes: 1024,
            required_free_bytes: 1,
        },
        Arc::new(FakeLauncher {
            mode: ProcessMode::Ready,
            stops: Arc::new(AtomicUsize::new(0)),
            specs: Arc::clone(&specs),
        }),
        Arc::new(FakeHealth { ready: true }),
        Arc::new(SlowStorage),
    )
    .expect("probe");
    let cancellation = ProbeCancellation::new();
    let cancelling = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancelling.cancel();
    });

    let report = probe
        .probe(fixture.fresh_workspace(), cancellation, &diagnostics())
        .await
        .expect("cancel report");

    assert_eq!(report.error_kind, Some(ProbeErrorKind::Cancelled));
    assert!(specs.lock().expect("specs").is_empty());
}

#[tokio::test]
async fn total_deadline_includes_slow_preflight() {
    let fixture = Fixture::new("deadline-preflight");
    let specs = Arc::new(Mutex::new(Vec::new()));
    let probe = test_probe_with_dependencies(
        ProbePolicy {
            timeout: Duration::from_millis(20),
            poll_interval: Duration::from_millis(10),
            stop_grace: Duration::ZERO,
            max_files: 64,
            max_candidate_bytes: 1024,
            required_free_bytes: 1,
        },
        Arc::new(FakeLauncher {
            mode: ProcessMode::Ready,
            stops: Arc::new(AtomicUsize::new(0)),
            specs: Arc::clone(&specs),
        }),
        Arc::new(FakeHealth { ready: true }),
        Arc::new(SlowStorage),
    )
    .expect("probe");

    let report = probe
        .probe(
            fixture.fresh_workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("deadline report");

    assert_eq!(report.error_kind, Some(ProbeErrorKind::ReadinessTimeout));
    assert!(
        report.elapsed_ms < 150,
        "preflight timeout must return promptly"
    );
    assert!(specs.lock().expect("specs").is_empty());
}

#[tokio::test]
async fn hardlinked_candidate_file_is_rejected_without_following_it() {
    let fixture = Fixture::new("hardlink");
    let outside = fixture.workspace.join("outside-secret");
    fs::write(&outside, b"secret").expect("outside file");
    fs::hard_link(&outside, fixture.candidate_dir.join("linked-secret"))
        .expect("hard link fixture");
    let (probe, stops, specs) = probe(ProcessMode::Ready, true);

    let report = probe
        .probe(
            fixture.workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("unsafe candidate is reported");

    assert_eq!(report.error_kind, Some(ProbeErrorKind::CandidateRejected));
    assert_eq!(stops.load(Ordering::SeqCst), 0);
    assert!(specs.lock().expect("spec lock").is_empty());
    assert_eq!(fs::read(outside).expect("outside retained"), b"secret");
}

#[tokio::test]
async fn empty_directory_fanout_counts_toward_candidate_entry_limit() {
    let fixture = Fixture::new("directory-limit");
    fs::create_dir_all(fixture.candidate_dir.join("one")).expect("first directory");
    fs::create_dir_all(fixture.candidate_dir.join("two")).expect("second directory");
    let stops = Arc::new(AtomicUsize::new(0));
    let specs = Arc::new(Mutex::new(Vec::new()));
    let probe = test_probe_with_dependencies(
        ProbePolicy {
            timeout: Duration::from_millis(500),
            poll_interval: Duration::from_millis(10),
            stop_grace: Duration::ZERO,
            // 状态文件位于 candidate 外；两个空目录恰好不应超过此上限。
            max_files: 2,
            max_candidate_bytes: 1024,
            required_free_bytes: 1,
        },
        Arc::new(FakeLauncher {
            mode: ProcessMode::Ready,
            stops,
            specs: Arc::clone(&specs),
        }),
        Arc::new(FakeHealth { ready: true }),
        Arc::new(PlentyOfSpace),
    )
    .expect("policy");

    let report = probe
        .probe(
            fixture.workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("entry limit is reported");

    assert_eq!(report.phase, ProbePhase::Passed);
    assert_eq!(specs.lock().expect("spec lock").len(), 1);
}

#[tokio::test]
async fn candidate_marker_from_a_different_trace_is_not_reused() {
    let fixture = Fixture::new("bad-state");
    fs::create_dir_all(
        fixture
            .layout
            .generation_root()
            .join(".state")
            .join(&fixture.candidate.id),
    )
    .expect("state dir");
    fs::write(
        fixture
            .layout
            .generation_root()
            .join(".state")
            .join(&fixture.candidate.id)
            .join("candidate.json"),
        br#"{"schema":1,"candidate_id":"candidate-001","runtime_version":"0.1.1-rc.1","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"candidate","trace_id":"old"}"#,
    )
    .expect("bad state");
    let (probe, stops, specs) = probe(ProcessMode::Ready, true);

    let error = probe
        .probe(
            fixture.workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect_err("a candidate marker cannot be rebound to a different trace");

    assert!(matches!(
        error,
        dsh_desktop_lib::update::probe::ProbeError::InvalidGenerationState
    ));
    assert_eq!(stops.load(Ordering::SeqCst), 0);
    assert!(specs.lock().expect("spec lock").is_empty());
}

struct UnsafePermissions;

impl ProbePermissionInspector for UnsafePermissions {
    fn ensure_private(&self, _path: &Path) -> Result<(), std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "broad write",
        ))
    }
}

#[tokio::test]
async fn unsafe_candidate_acl_is_rejected_before_process_launch() {
    let fixture = Fixture::new("unsafe-acl");
    let specs = Arc::new(Mutex::new(Vec::new()));
    let probe = RuntimeProbe::with_inspectors(
        ProbePolicy {
            timeout: Duration::from_millis(500),
            poll_interval: Duration::from_millis(10),
            stop_grace: Duration::ZERO,
            max_files: 64,
            max_candidate_bytes: 1024,
            required_free_bytes: 1,
        },
        Arc::new(FakeLauncher {
            mode: ProcessMode::Ready,
            stops: Arc::new(AtomicUsize::new(0)),
            specs: Arc::clone(&specs),
        }),
        Arc::new(FakeHealth { ready: true }),
        Arc::new(PlentyOfSpace),
        Arc::new(UnsafePermissions),
    )
    .expect("policy");

    let report = probe
        .probe(
            fixture.fresh_workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("acl failure report");

    assert_eq!(report.error_kind, Some(ProbeErrorKind::CandidateRejected));
    assert!(specs.lock().expect("specs").is_empty());
}

#[tokio::test]
async fn passed_state_reader_rejects_runtime_or_trace_substitution() {
    let fixture = Fixture::new("passed-reader");
    let (probe, _, _) = probe(ProcessMode::Ready, true);
    let report = probe
        .probe(
            fixture.fresh_workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("probe");
    assert_eq!(report.phase, ProbePhase::Passed);
    let trace = report.trace_id.as_str();
    assert!(
        read_passed_generation_state(&fixture.layout, &fixture.candidate, &fixture.runtime, trace,)
            .is_ok()
    );
    let other =
        InstalledRuntime::with_node_version("0.1.2", "b".repeat(64), "24.15.0").expect("runtime");
    assert!(
        read_passed_generation_state(&fixture.layout, &fixture.candidate, &other, trace).is_err()
    );
    assert!(
        read_passed_generation_state(
            &fixture.layout,
            &fixture.candidate,
            &fixture.runtime,
            "trace_other",
        )
        .is_err()
    );
}

struct PanicHealth;

impl ReadyProbe for PanicHealth {
    fn wait_until_ready(&self, _port: u16, _timeout: Duration) -> Result<String, RuntimeError> {
        panic!("health worker panic")
    }
}

struct DropTrackedProcess {
    drops: Arc<AtomicUsize>,
}

impl Drop for DropTrackedProcess {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

impl RuntimeProcess for DropTrackedProcess {
    fn id(&self) -> u32 {
        7
    }
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, RuntimeError> {
        Ok(None)
    }
    fn stop(&mut self, _grace: Duration) -> Result<StopOutcome, RuntimeError> {
        Ok(StopOutcome::Terminated)
    }
    fn wait_for_readiness(
        &mut self,
        port: u16,
        _timeout: Duration,
    ) -> Result<ReadinessSignal, RuntimeError> {
        Ok(ReadinessSignal::Web { port })
    }
}

struct DropTrackedLauncher {
    drops: Arc<AtomicUsize>,
}

impl ProcessLauncher for DropTrackedLauncher {
    fn spawn(&self, _spec: &RuntimeLaunchSpec) -> Result<Box<dyn RuntimeProcess>, RuntimeError> {
        Ok(Box::new(DropTrackedProcess {
            drops: Arc::clone(&self.drops),
        }))
    }
}

#[tokio::test]
async fn panicking_health_worker_still_drops_managed_process() {
    let fixture = Fixture::new("panic-drop");
    let drops = Arc::new(AtomicUsize::new(0));
    let probe = test_probe_with_dependencies(
        ProbePolicy {
            timeout: Duration::from_millis(500),
            poll_interval: Duration::from_millis(10),
            stop_grace: Duration::ZERO,
            max_files: 64,
            max_candidate_bytes: 1024,
            required_free_bytes: 1,
        },
        Arc::new(DropTrackedLauncher {
            drops: Arc::clone(&drops),
        }),
        Arc::new(PanicHealth),
        Arc::new(PlentyOfSpace),
    )
    .expect("probe");

    let report = probe
        .probe(
            fixture.fresh_workspace(),
            ProbeCancellation::new(),
            &diagnostics(),
        )
        .await
        .expect("panic is stable report");

    assert_eq!(report.error_kind, Some(ProbeErrorKind::WorkerFailed));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn candidate_directory_reparse_point_is_rejected_at_workspace_boundary() {
    use std::os::windows::fs::symlink_dir;

    let fixture = Fixture::new("candidate-reparse");
    let candidate = DataGeneration::new("candidate-linked").expect("candidate");
    let outside = fixture.workspace.join("outside-generation");
    fs::create_dir_all(&outside).expect("outside generation");
    let linked = fixture.layout.generation_dir(&candidate);
    if symlink_dir(&outside, &linked).is_err() {
        // 未启用 Windows Developer Mode 的机器可能禁止普通用户创建符号链接；
        // 生产检查仍由文件属性实现，其他边界测试不依赖此系统权限。
        return;
    }

    let error = ProbeWorkspace::new(
        fixture.layout,
        fixture.runtime,
        Version::parse("24.15.0").expect("node version"),
        candidate,
        fixture.workspace,
        &ProbeLease::for_test(),
    )
    .expect_err("candidate reparse point must fail closed");

    assert!(
        matches!(
            error,
            dsh_desktop_lib::update::probe::ProbeError::UnsafeBoundary
        ),
        "unexpected error: {error:?}"
    );
}

#[test]
fn probe_rejects_node_version_not_bound_to_persisted_runtime_descriptor() {
    let fixture = Fixture::new("node-descriptor-mismatch");
    InstallStateStore::new(fixture.layout.clone())
        .save(&ActiveDeployment::with_project_workspace(
            fixture.runtime.clone(),
            fixture.active.clone(),
            "2026-08-22T00:00:00Z".to_owned(),
            fixture.workspace.clone(),
        ))
        .expect("active deployment");

    let error = ProbeWorkspace::new(
        fixture.layout,
        fixture.runtime,
        Version::parse("23.0.0").expect("node version"),
        fixture.candidate,
        fixture.workspace,
        &ProbeLease::for_test(),
    )
    .expect_err("probe Node must match the persisted runtime descriptor");

    assert!(matches!(
        error,
        dsh_desktop_lib::update::probe::ProbeError::RuntimeDescriptorMismatch
    ));
}

#[test]
fn active_candidate_is_loaded_from_trusted_deployment_and_rejected() {
    let fixture = Fixture::new("active-derived");
    InstallStateStore::new(fixture.layout.clone())
        .save(&ActiveDeployment::with_project_workspace(
            fixture.runtime.clone(),
            fixture.candidate.clone(),
            "2026-08-22T00:00:00Z".to_owned(),
            fixture.workspace.clone(),
        ))
        .expect("active deployment");

    let error = ProbeWorkspace::new(
        fixture.layout,
        fixture.runtime,
        Version::parse("24.15.0").expect("node version"),
        fixture.candidate,
        fixture.workspace,
        &ProbeLease::for_test(),
    )
    .expect_err("trusted active candidate must be rejected");

    assert!(matches!(
        error,
        dsh_desktop_lib::update::probe::ProbeError::CandidateIsActive
    ));
}

#[test]
fn cloned_lease_cannot_construct_two_probe_workspaces_concurrently() {
    let fixture = Fixture::new("lease-claim");
    let lease = ProbeLease::for_test();
    let clone = lease.clone();
    let first = ProbeWorkspace::new(
        fixture.layout.clone(),
        fixture.runtime.clone(),
        Version::parse("24.15.0").expect("node"),
        fixture.candidate.clone(),
        fixture.workspace.clone(),
        &lease,
    )
    .expect("first workspace");

    let second = ProbeWorkspace::new(
        fixture.layout.clone(),
        fixture.runtime.clone(),
        Version::parse("24.15.0").expect("node"),
        fixture.candidate.clone(),
        fixture.workspace.clone(),
        &clone,
    )
    .expect_err("second concurrent workspace must be rejected");
    assert!(matches!(
        second,
        dsh_desktop_lib::update::probe::ProbeError::ProbeAlreadyActive
    ));

    drop(first);
    ProbeWorkspace::new(
        fixture.layout,
        fixture.runtime,
        Version::parse("24.15.0").expect("node"),
        fixture.candidate,
        fixture.workspace,
        &clone,
    )
    .expect("permit should be reusable after drop");
}

#[test]
fn policy_rejects_unbounded_deadlines_before_health_probe_can_overflow() {
    let result = test_probe_with_dependencies(
        ProbePolicy {
            timeout: Duration::MAX,
            poll_interval: Duration::MAX,
            ..ProbePolicy::default()
        },
        Arc::new(FakeLauncher {
            mode: ProcessMode::Ready,
            stops: Arc::new(AtomicUsize::new(0)),
            specs: Arc::new(Mutex::new(Vec::new())),
        }),
        Arc::new(FakeHealth { ready: true }),
        Arc::new(PlentyOfSpace),
    );

    assert!(matches!(
        result,
        Err(dsh_desktop_lib::update::probe::ProbeError::InvalidPolicy { field: "timeout" })
    ));
}

#[test]
fn policy_rejects_poll_intervals_above_cancellation_latency_bound() {
    let result = test_probe_with_dependencies(
        ProbePolicy {
            timeout: Duration::from_secs(5),
            poll_interval: Duration::from_millis(1_001),
            ..ProbePolicy::default()
        },
        Arc::new(FakeLauncher {
            mode: ProcessMode::Ready,
            stops: Arc::new(AtomicUsize::new(0)),
            specs: Arc::new(Mutex::new(Vec::new())),
        }),
        Arc::new(FakeHealth { ready: true }),
        Arc::new(PlentyOfSpace),
    );

    assert!(matches!(
        result,
        Err(dsh_desktop_lib::update::probe::ProbeError::InvalidPolicy {
            field: "poll_interval"
        })
    ));
}
