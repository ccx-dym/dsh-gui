#![cfg(windows)]

use dsh_desktop_lib::paths::{AppPaths, RuntimeLayout};
use dsh_desktop_lib::runtime::command::RuntimeLaunchSpec;
use dsh_desktop_lib::runtime::health::ReadyProbe;
use dsh_desktop_lib::runtime::install_state::{DataGeneration, InstalledRuntime};
use dsh_desktop_lib::runtime::process::StopOutcome;
use dsh_desktop_lib::runtime::{ProcessLauncher, ReadinessSignal, RuntimeError, RuntimeProcess};
use dsh_desktop_lib::update::probe::{
    ProbeCancellation, ProbeErrorKind, ProbePhase, ProbePolicy, ProbeStorageInspector,
    ProbeWorkspace, RuntimeProbe, RuntimeStoppedState,
};
use semver::Version;
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

struct NoSpace;

impl ProbeStorageInspector for NoSpace {
    fn available_bytes(&self, _path: &Path) -> Result<u64, std::io::Error> {
        std::thread::sleep(Duration::from_millis(15));
        Ok(0)
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
        let runtime = InstalledRuntime::new("0.1.1-rc.1", "a".repeat(64)).expect("runtime");
        let candidate = DataGeneration::new("candidate-001").expect("candidate");
        let active = DataGeneration::new("active-001").expect("active");
        let runtime_dir = layout.runtime_dir(&runtime);
        let node_dir = runtime_dir.join("node-v24.15.0-win-x64");
        let cli_dir = runtime_dir.join("app/node_modules/@deepseek-ai/dsh/lib");
        fs::create_dir_all(&node_dir).expect("node dir");
        fs::create_dir_all(&cli_dir).expect("cli dir");
        fs::write(node_dir.join("node.exe"), b"node").expect("node");
        fs::write(cli_dir.join("bin.js"), b"export {}").expect("cli");
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
        }
    }

    fn workspace(&self) -> ProbeWorkspace {
        ProbeWorkspace::new(
            self.layout.clone(),
            self.runtime.clone(),
            Version::parse("24.15.0").expect("node version"),
            self.candidate.clone(),
            Some(self.active.clone()),
            self.workspace.clone(),
            RuntimeStoppedState::ConfirmedStopped,
        )
        .expect("probe workspace")
    }

    fn fresh_workspace(&self) -> ProbeWorkspace {
        ProbeWorkspace::new(
            self.layout.clone(),
            self.runtime.clone(),
            Version::parse("24.15.0").expect("node version"),
            self.candidate.clone(),
            None,
            self.workspace.clone(),
            RuntimeStoppedState::ConfirmedStopped,
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
    let probe = RuntimeProbe::with_dependencies(
        ProbePolicy {
            timeout: Duration::from_millis(80),
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
            "trace_success".to_owned(),
            ProbeCancellation::new(),
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
            .candidate_dir
            .join("generation-state-passed.json")
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
async fn stdout_without_http_is_invalid_webui_and_reclaims_process_tree() {
    let fixture = Fixture::new("invalid-webui");
    let (probe, stops, _) = probe(ProcessMode::Ready, false);

    let report = probe
        .probe(
            fixture.workspace(),
            "trace_http".to_owned(),
            ProbeCancellation::new(),
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
            "trace_stdout".to_owned(),
            ProbeCancellation::new(),
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
            "trace_exit".to_owned(),
            ProbeCancellation::new(),
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
    cancellation.cancel();

    let report = probe
        .probe(fixture.workspace(), "trace_cancel".to_owned(), cancellation)
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
            "trace_cleanup".to_owned(),
            ProbeCancellation::new(),
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
    let probe = RuntimeProbe::with_dependencies(
        ProbePolicy {
            timeout: Duration::from_millis(50),
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
            "trace_space".to_owned(),
            ProbeCancellation::new(),
        )
        .await
        .expect("preflight is reported");

    assert_eq!(report.phase, ProbePhase::Failed);
    assert_eq!(report.error_kind, Some(ProbeErrorKind::CandidateRejected));
    assert!(report.elapsed_ms >= 10);
    assert!(specs.lock().expect("spec lock").is_empty());
    assert!(
        fixture
            .candidate_dir
            .join("generation-state-failed.json")
            .is_file()
    );
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
            "trace_link".to_owned(),
            ProbeCancellation::new(),
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
    let probe = RuntimeProbe::with_dependencies(
        ProbePolicy {
            timeout: Duration::from_millis(50),
            poll_interval: Duration::from_millis(10),
            stop_grace: Duration::ZERO,
            // Candidate 状态文件与两个空目录已经超过此总 entry 上限。
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
            "trace_entries".to_owned(),
            ProbeCancellation::new(),
        )
        .await
        .expect("entry limit is reported");

    assert_eq!(report.error_kind, Some(ProbeErrorKind::CandidateRejected));
    assert!(specs.lock().expect("spec lock").is_empty());
}

#[tokio::test]
async fn malformed_existing_candidate_state_is_not_treated_as_trusted() {
    let fixture = Fixture::new("bad-state");
    fs::write(
        fixture
            .candidate_dir
            .join("generation-state-candidate.json"),
        br#"{"schema":99,"state":"candidate","trace_id":"old"}"#,
    )
    .expect("bad state");
    let (probe, stops, specs) = probe(ProcessMode::Ready, true);

    let error = probe
        .probe(
            fixture.workspace(),
            "trace_state".to_owned(),
            ProbeCancellation::new(),
        )
        .await
        .expect_err("unknown state schema must fail closed");

    assert!(matches!(
        error,
        dsh_desktop_lib::update::probe::ProbeError::InvalidGenerationState
    ));
    assert_eq!(stops.load(Ordering::SeqCst), 0);
    assert!(specs.lock().expect("spec lock").is_empty());
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
        Some(fixture.active),
        fixture.workspace,
        RuntimeStoppedState::ConfirmedStopped,
    )
    .expect_err("candidate reparse point must fail closed");

    assert!(matches!(
        error,
        dsh_desktop_lib::update::probe::ProbeError::UnsafeBoundary
    ));
}

#[test]
fn policy_rejects_unbounded_deadlines_before_health_probe_can_overflow() {
    let result = RuntimeProbe::with_dependencies(
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
