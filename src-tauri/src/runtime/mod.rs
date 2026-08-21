pub(crate) mod atomic_file;
pub mod command;
pub mod health;
pub mod install_state;
#[cfg(windows)]
pub mod process;

use std::io;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::domain::RuntimeEvent;
use command::{ReadinessPolicy, RuntimeLaunchSpec};
use health::{HealthProbe, ReadyProbe};
#[cfg(windows)]
use process::{ManagedChild, StopOutcome};

/// DSH 本地运行时生命周期中的可诊断错误。
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("运行时 I/O 失败: {0}")]
    Io(#[from] io::Error),
    #[error("DSH 在 {timeout_ms} ms 内未通过端口 {port} 探活")]
    HealthTimeout { port: u16, timeout_ms: u64 },
    #[error("运行时已经启动")]
    AlreadyRunning,
    #[error("Windows 进程管理失败（{operation}，HRESULT {code:#010X}）")]
    Process { operation: &'static str, code: i32 },
    #[error("缺少 main 窗口")]
    MainWindowMissing,
    #[error("无效的本地运行时 URL: {0}")]
    InvalidUrl(String),
    #[error("Tauri 操作失败: {0}")]
    Tauri(String),
    #[error("正式构建禁止启动模拟运行时")]
    MockRuntimeDisabled,
    #[error("运行时启动参数缺少回环端口")]
    MissingLoopbackPort,
    #[error("运行时状态锁已损坏")]
    StatePoisoned,
    #[error("运行时进程在就绪前退出")]
    ProcessExitedEarly,
    #[error("DSH 在 {timeout_ms} ms 内未打印端口 {port} 的可信就绪信号")]
    OutputReadinessTimeout { port: u16, timeout_ms: u64 },
    #[error("运行时启动路径无效（{field}: {reason}）")]
    InvalidLaunchPath {
        field: &'static str,
        reason: &'static str,
    },
    #[error("无效的动态回环端口: {port}")]
    InvalidLoopbackPort { port: u16 },
    #[error("探活前必须先完整停止当前 DSH")]
    ProbeRequiresStoppedRuntime,
    #[error("运行时更新或探活操作正在进行")]
    ProbeOperationInProgress,
    #[error("当前 Agent 活动状态无法安全进入运行时激活")]
    ActivationBusy,
    #[error("激活指针已变化，拒绝启动不匹配的 runtime/data 配对")]
    DeploymentChanged,
    #[error("当前启动进程没有可用的强制终止句柄")]
    StartupAbortUnavailable,
    #[error("强制终止启动进程后状态未在 {timeout_ms} ms 内收敛")]
    StartupAbortTimeout { timeout_ms: u64 },
    #[error("运行时启动已被激活事务强制终止")]
    StartupAborted,
}

impl RuntimeError {
    /// 返回供前端诊断和自动化判断使用的稳定错误码。
    ///
    /// :return: 不含动态路径、端口或错误正文的 snake_case 错误码。
    /// :raises: 此纯映射不产生错误。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "io_error",
            Self::HealthTimeout { .. } => "health_timeout",
            Self::AlreadyRunning => "already_running",
            Self::Process { .. } => "process_error",
            Self::MainWindowMissing => "main_window_missing",
            Self::InvalidUrl(_) => "invalid_url",
            Self::Tauri(_) => "tauri_error",
            Self::MockRuntimeDisabled => "mock_runtime_disabled",
            Self::MissingLoopbackPort => "missing_loopback_port",
            Self::StatePoisoned => "state_poisoned",
            Self::ProcessExitedEarly => "process_exited_early",
            Self::OutputReadinessTimeout { .. } => "output_readiness_timeout",
            Self::InvalidLaunchPath { .. } => "invalid_launch_path",
            Self::InvalidLoopbackPort { .. } => "invalid_loopback_port",
            Self::ProbeRequiresStoppedRuntime => "probe_requires_stopped_runtime",
            Self::ProbeOperationInProgress => "probe_operation_in_progress",
            Self::ActivationBusy => "activation_busy",
            Self::DeploymentChanged => "deployment_changed",
            Self::StartupAbortUnavailable => "startup_abort_unavailable",
            Self::StartupAbortTimeout { .. } => "startup_abort_timeout",
            Self::StartupAborted => "startup_aborted",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// 从受管 stdout 中提取的可信控制信号。
pub enum ReadinessSignal {
    /// 官方 DSH stdout 声明的回环 Web 端口。
    Web { port: u16 },
}

/// 抽象受管子进程，使状态机测试不依赖真实 Windows 进程。
///
/// 实现必须在 `Drop` 中尽力回收完整进程树；调用方可在正常路径显式调用
/// `stop` 获取结果，但 worker panic 或提前返回时仍不得遗留后台进程。
pub trait RuntimeProcess: Send {
    fn id(&self) -> u32;
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, RuntimeError>;
    fn stop(&mut self, grace: Duration) -> Result<StopOutcome, RuntimeError>;
    fn wait_for_readiness(
        &mut self,
        port: u16,
        timeout: Duration,
    ) -> Result<ReadinessSignal, RuntimeError>;

    fn startup_abort(&self) -> Option<Arc<dyn StartupAbort>> {
        None
    }
}

pub trait StartupAbort: Send + Sync {
    fn abort(&self) -> Result<(), RuntimeError>;
}

/// 创建受生命周期约束的运行时进程。
pub trait ProcessLauncher: Send + Sync {
    fn spawn(&self, spec: &RuntimeLaunchSpec) -> Result<Box<dyn RuntimeProcess>, RuntimeError>;
}

#[derive(Clone, Copy, Debug, Default)]
/// 使用 Windows Job Object 启动进程树的生产 launcher。
pub struct WindowsProcessLauncher;

#[cfg(windows)]
impl ProcessLauncher for WindowsProcessLauncher {
    fn spawn(&self, spec: &RuntimeLaunchSpec) -> Result<Box<dyn RuntimeProcess>, RuntimeError> {
        Ok(Box::new(ManagedChild::spawn(spec)?))
    }
}

#[cfg(windows)]
impl RuntimeProcess for ManagedChild {
    fn id(&self) -> u32 {
        self.id()
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, RuntimeError> {
        self.try_wait()
    }

    fn stop(&mut self, grace: Duration) -> Result<StopOutcome, RuntimeError> {
        self.stop(grace)
    }

    fn wait_for_readiness(
        &mut self,
        port: u16,
        timeout: Duration,
    ) -> Result<ReadinessSignal, RuntimeError> {
        self.wait_for_readiness(port, timeout)
    }

    fn startup_abort(&self) -> Option<Arc<dyn StartupAbort>> {
        self.startup_abort_handle()
            .map(|handle| Arc::new(handle) as Arc<dyn StartupAbort>)
            .ok()
    }
}

/// 把类型化运行时事件发送给状态存储和界面边界。
pub trait RuntimeEventSink: Send + Sync + 'static {
    fn emit(&self, event: RuntimeEvent) -> Result<(), RuntimeError>;
}

enum SupervisorState {
    Stopped,
    Starting {
        abort: Option<Arc<dyn StartupAbort>>,
        abort_requested: bool,
    },
    Running {
        child: Box<dyn RuntimeProcess>,
        url: String,
        sink: Arc<dyn RuntimeEventSink>,
    },
    Stopping,
    Failed,
}

/// 串行化生命周期状态转换，并把阻塞工作隔离到状态锁之外。
pub struct RuntimeSupervisor {
    state: Arc<Mutex<SupervisorState>>,
    launcher: Arc<dyn ProcessLauncher>,
    probe: Arc<dyn ReadyProbe>,
}

impl RuntimeSupervisor {
    /// 创建使用 Windows Job Object 与真实 HTTP 探活的运行时协调器。
    ///
    /// :return: 初始状态为停止的协调器。
    /// :raises: 此构造函数不产生错误。
    pub fn new() -> Self {
        Self::with_dependencies(Arc::new(WindowsProcessLauncher), Arc::new(HealthProbe))
    }

    fn with_dependencies(launcher: Arc<dyn ProcessLauncher>, probe: Arc<dyn ReadyProbe>) -> Self {
        Self {
            state: Arc::new(Mutex::new(SupervisorState::Stopped)),
            launcher,
            probe,
        }
    }

    #[cfg(test)]
    fn for_test(launcher: Arc<dyn ProcessLauncher>, probe: Arc<dyn ReadyProbe>) -> Self {
        Self::with_dependencies(launcher, probe)
    }

    /// 原子占用启动权，并把进程创建与探活调度到专用后台线程。
    ///
    /// :param spec: 不经 shell 解析的运行时启动参数。
    /// :param timeout: HTTP 探活的总截止时间。
    /// :param sink: 接收完整生命周期事件的线程安全事件出口。
    /// :return: 后台任务成功调度时立即返回；不等待进程或探活。
    /// :raises RuntimeError: 已在启动/运行、状态锁损坏、起始事件发送失败或线程
    ///   无法创建时返回结构化错误。
    pub fn start(
        &self,
        spec: RuntimeLaunchSpec,
        timeout: Duration,
        sink: Arc<dyn RuntimeEventSink>,
    ) -> Result<(), RuntimeError> {
        {
            let mut state = self.state.lock().map_err(|_| RuntimeError::StatePoisoned)?;
            match &*state {
                SupervisorState::Stopped | SupervisorState::Failed => {
                    *state = SupervisorState::Starting {
                        abort: None,
                        abort_requested: false,
                    };
                }
                SupervisorState::Starting { .. }
                | SupervisorState::Running { .. }
                | SupervisorState::Stopping => {
                    return Err(RuntimeError::AlreadyRunning);
                }
            }
        }

        let starting = RuntimeEvent::Starting {
            message: "正在启动 DSH".to_owned(),
        };
        if let Err(error) = sink.emit(starting) {
            self.set_failed()?;
            emit_failure(&sink, &error);
            return Err(error);
        }

        let state = Arc::clone(&self.state);
        let launcher = Arc::clone(&self.launcher);
        let probe = Arc::clone(&self.probe);
        let worker_sink = Arc::clone(&sink);
        let spawn_result = thread::Builder::new()
            .name("dsh-runtime-start".to_owned())
            .spawn(move || run_start(state, launcher, probe, spec, timeout, worker_sink));

        if let Err(error) = spawn_result {
            let error = RuntimeError::Io(error);
            self.set_failed()?;
            emit_failure(&sink, &error);
            return Err(error);
        }
        Ok(())
    }

    /// 停止当前已就绪运行时；所有可能阻塞的操作都在释放状态锁后执行。
    ///
    /// :param grace: 强制结束进程树前的自然退出宽限期。
    /// :return: 运行时已停止或本来已停止时返回 `Ok(())`。
    /// :raises RuntimeError: 状态锁损坏、启动仍在进行或进程停止失败时返回。
    pub fn stop(&self, grace: Duration) -> Result<(), RuntimeError> {
        let running = {
            let mut state = self.state.lock().map_err(|_| RuntimeError::StatePoisoned)?;
            match std::mem::replace(&mut *state, SupervisorState::Stopping) {
                SupervisorState::Running { child, url, sink } => Some((child, url, sink)),
                starting @ SupervisorState::Starting { .. } => {
                    *state = starting;
                    return Err(RuntimeError::AlreadyRunning);
                }
                SupervisorState::Stopping => {
                    *state = SupervisorState::Stopping;
                    return Err(RuntimeError::AlreadyRunning);
                }
                SupervisorState::Failed | SupervisorState::Stopped => {
                    *state = SupervisorState::Stopped;
                    None
                }
            }
        };

        let Some((mut child, _url, sink)) = running else {
            return Ok(());
        };
        let sink_error = sink
            .emit(RuntimeEvent::Stopping {
                message: "正在停止 DSH".to_owned(),
            })
            .err();
        let stop_error = child.stop(grace).err();
        if let Some(error) = stop_error.or(sink_error) {
            self.set_failed()?;
            emit_failure(&sink, &error);
            return Err(error);
        }
        let mut state = self.state.lock().map_err(|_| RuntimeError::StatePoisoned)?;
        *state = SupervisorState::Stopped;
        Ok(())
    }

    /// 返回 supervisor 是否权威确认没有 Starting/Running/Stopping 进程。
    ///
    /// :return: `Stopped` 或已完成清理的 `Failed` 状态返回 true。
    /// :raises RuntimeError: 状态锁损坏时返回。
    pub fn is_inactive(&self) -> Result<bool, RuntimeError> {
        let state = self.state.lock().map_err(|_| RuntimeError::StatePoisoned)?;
        Ok(matches!(
            *state,
            SupervisorState::Stopped | SupervisorState::Failed
        ))
    }

    /// 强制终止仍处于 Starting 的受管进程树，并等待状态机收敛。
    ///
    /// :param timeout: 终止句柄执行后等待 worker 收敛的最大时长。
    /// :return: 进程树已被终止且状态离开 Starting 时返回。
    /// :raises RuntimeError: 当前不是 Starting、终止句柄不可用或状态未收敛时返回。
    #[cfg_attr(debug_assertions, allow(dead_code))]
    pub(crate) fn abort_startup(&self, timeout: Duration) -> Result<(), RuntimeError> {
        let abort = {
            let mut state = self.state.lock().map_err(|_| RuntimeError::StatePoisoned)?;
            match &mut *state {
                SupervisorState::Starting {
                    abort,
                    abort_requested,
                } => {
                    *abort_requested = true;
                    abort.clone().ok_or(RuntimeError::StartupAbortUnavailable)?
                }
                _ => return Err(RuntimeError::AlreadyRunning),
            }
        };
        abort.abort()?;
        let deadline = Instant::now() + timeout;
        loop {
            let starting = matches!(
                *self.state.lock().map_err(|_| RuntimeError::StatePoisoned)?,
                SupervisorState::Starting { .. }
            );
            if !starting {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(RuntimeError::StartupAbortTimeout {
                    timeout_ms: timeout.as_millis() as u64,
                });
            }
            thread::sleep(Duration::from_millis(10).min(timeout));
        }
    }

    fn set_failed(&self) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().map_err(|_| RuntimeError::StatePoisoned)?;
        *state = SupervisorState::Failed;
        Ok(())
    }
}

impl Default for RuntimeSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

fn run_start(
    state: Arc<Mutex<SupervisorState>>,
    launcher: Arc<dyn ProcessLauncher>,
    probe: Arc<dyn ReadyProbe>,
    spec: RuntimeLaunchSpec,
    timeout: Duration,
    sink: Arc<dyn RuntimeEventSink>,
) {
    let started = Instant::now();
    let port = match spec.loopback_port {
        Some(port) => port,
        None => {
            fail_start(&state, &sink, None, RuntimeError::MissingLoopbackPort);
            return;
        }
    };
    let mut child = match launcher.spawn(&spec) {
        Ok(child) => child,
        Err(error) => {
            fail_start(&state, &sink, None, error);
            return;
        }
    };
    let abort_requested = match state.lock() {
        Ok(mut current) => match &mut *current {
            SupervisorState::Starting {
                abort,
                abort_requested,
            } => {
                *abort = child.startup_abort();
                *abort_requested
            }
            _ => true,
        },
        Err(_) => true,
    };
    if abort_requested {
        fail_start(&state, &sink, Some(child), RuntimeError::StartupAborted);
        return;
    }
    match child.try_wait() {
        Ok(Some(_)) => {
            fail_start(&state, &sink, Some(child), RuntimeError::ProcessExitedEarly);
            return;
        }
        Err(error) => {
            fail_start(&state, &sink, Some(child), error);
            return;
        }
        Ok(None) => {}
    }

    if spec.readiness_policy == ReadinessPolicy::StdoutAndHttp {
        let remaining = timeout.saturating_sub(started.elapsed());
        if let Err(error) = child.wait_for_readiness(port, remaining) {
            fail_start(&state, &sink, Some(child), error);
            return;
        }
    }

    let remaining = timeout.saturating_sub(started.elapsed());
    let url = match probe.wait_until_ready(port, remaining) {
        Ok(url) => url,
        Err(error) => {
            fail_start(&state, &sink, Some(child), error);
            return;
        }
    };
    let aborted = match state.lock() {
        Ok(current) => !matches!(
            &*current,
            SupervisorState::Starting {
                abort_requested: false,
                ..
            }
        ),
        Err(_) => true,
    };
    if aborted {
        fail_start(&state, &sink, Some(child), RuntimeError::StartupAborted);
        return;
    }
    let event = RuntimeEvent::Ready {
        url: url.clone(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    };
    if let Err(error) = sink.emit(event) {
        fail_start(&state, &sink, Some(child), error);
        return;
    }

    match state.lock() {
        Ok(mut current)
            if matches!(
                &*current,
                SupervisorState::Starting {
                    abort_requested: false,
                    ..
                }
            ) =>
        {
            *current = SupervisorState::Running { child, url, sink };
        }
        Ok(mut current) => {
            let mut child = child;
            let error = RuntimeError::StartupAborted;
            if let Err(stop_error) = child.stop(Duration::ZERO) {
                *current = SupervisorState::Failed;
                emit_failure(&sink, &stop_error);
                return;
            }
            *current = SupervisorState::Failed;
            emit_failure(&sink, &error);
        }
        Err(_) => {
            let mut child = child;
            let error = RuntimeError::StatePoisoned;
            let _ = child.stop(Duration::ZERO);
            emit_failure(&sink, &error);
        }
    }
}

fn fail_start(
    state: &Arc<Mutex<SupervisorState>>,
    sink: &Arc<dyn RuntimeEventSink>,
    mut child: Option<Box<dyn RuntimeProcess>>,
    mut error: RuntimeError,
) {
    if let Some(process) = child.as_mut()
        && let Err(stop_error) = process.stop(Duration::ZERO)
    {
        error = stop_error;
    }
    if let Ok(mut current) = state.lock() {
        *current = SupervisorState::Failed;
    }
    emit_failure(sink, &error);
}

fn emit_failure(sink: &Arc<dyn RuntimeEventSink>, error: &RuntimeError) {
    let _ = sink.emit(RuntimeEvent::Failed {
        code: error.code().to_owned(),
        message: error.to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::{
        ProcessLauncher, ReadinessSignal, RuntimeError, RuntimeEventSink, RuntimeProcess,
        RuntimeSupervisor, StartupAbort, SupervisorState,
    };
    use crate::domain::RuntimeEvent;
    use crate::runtime::command::{ReadinessPolicy, RuntimeLaunchSpec};
    use crate::runtime::health::ReadyProbe;
    use crate::runtime::process::StopOutcome;
    use std::path::PathBuf;
    use std::process::ExitStatus;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    #[derive(Clone, Default)]
    struct RecordingSink {
        events: Arc<(Mutex<Vec<RuntimeEvent>>, Condvar)>,
    }

    impl RecordingSink {
        fn wait_for_count(&self, count: usize, timeout: Duration) -> Vec<RuntimeEvent> {
            let deadline = Instant::now() + timeout;
            let (events, changed) = &*self.events;
            let mut values = events.lock().expect("事件锁不应中毒");
            while values.len() < count {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "等待运行时事件超时");
                let (next, result) = changed
                    .wait_timeout(values, remaining)
                    .expect("事件锁不应中毒");
                values = next;
                assert!(
                    !result.timed_out() || values.len() >= count,
                    "等待运行时事件超时"
                );
            }
            values.clone()
        }
    }

    impl RuntimeEventSink for RecordingSink {
        fn emit(&self, event: RuntimeEvent) -> Result<(), RuntimeError> {
            let (events, changed) = &*self.events;
            events.lock().expect("事件锁不应中毒").push(event);
            changed.notify_all();
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FailReadySink {
        events: RecordingSink,
    }

    #[derive(Clone, Default)]
    struct FailStoppingSink {
        events: RecordingSink,
    }

    impl RuntimeEventSink for FailStoppingSink {
        fn emit(&self, event: RuntimeEvent) -> Result<(), RuntimeError> {
            self.events.emit(event.clone())?;
            if matches!(event, RuntimeEvent::Stopping { .. }) {
                return Err(RuntimeError::Tauri("停止事件发送失败".to_owned()));
            }
            Ok(())
        }
    }

    impl RuntimeEventSink for FailReadySink {
        fn emit(&self, event: RuntimeEvent) -> Result<(), RuntimeError> {
            self.events.emit(event.clone())?;
            if matches!(event, RuntimeEvent::Ready { .. }) {
                return Err(RuntimeError::MainWindowMissing);
            }
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FakeProcessState {
        stop_calls: Arc<Mutex<usize>>,
    }

    struct FakeProcess {
        state: FakeProcessState,
    }

    impl RuntimeProcess for FakeProcess {
        fn id(&self) -> u32 {
            42
        }

        fn try_wait(&mut self) -> Result<Option<ExitStatus>, RuntimeError> {
            Ok(None)
        }

        fn stop(&mut self, _grace: Duration) -> Result<StopOutcome, RuntimeError> {
            *self.state.stop_calls.lock().expect("停止计数锁不应中毒") += 1;
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

    struct FakeLauncher {
        state: FakeProcessState,
    }

    struct ReadinessFailProcess {
        state: FakeProcessState,
    }

    impl RuntimeProcess for ReadinessFailProcess {
        fn id(&self) -> u32 {
            126
        }

        fn try_wait(&mut self) -> Result<Option<ExitStatus>, RuntimeError> {
            Ok(None)
        }

        fn stop(&mut self, _grace: Duration) -> Result<StopOutcome, RuntimeError> {
            *self.state.stop_calls.lock().expect("停止计数锁不应中毒") += 1;
            Ok(StopOutcome::Terminated)
        }

        fn wait_for_readiness(
            &mut self,
            port: u16,
            _timeout: Duration,
        ) -> Result<ReadinessSignal, RuntimeError> {
            Err(RuntimeError::OutputReadinessTimeout {
                port,
                timeout_ms: 1,
            })
        }
    }

    struct ReadinessFailLauncher {
        state: FakeProcessState,
    }

    impl ProcessLauncher for ReadinessFailLauncher {
        fn spawn(
            &self,
            _spec: &RuntimeLaunchSpec,
        ) -> Result<Box<dyn RuntimeProcess>, RuntimeError> {
            Ok(Box::new(ReadinessFailProcess {
                state: self.state.clone(),
            }))
        }
    }

    #[derive(Clone, Default)]
    struct StopGate {
        state: Arc<(Mutex<(bool, bool)>, Condvar)>,
    }

    impl StopGate {
        fn wait_until_entered(&self, timeout: Duration) {
            let deadline = Instant::now() + timeout;
            let (state, changed) = &*self.state;
            let mut values = state.lock().expect("停止闸锁不应中毒");
            while !values.0 {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "等待 stop 进入超时");
                let (next, result) = changed
                    .wait_timeout(values, remaining)
                    .expect("停止闸锁不应中毒");
                values = next;
                assert!(!result.timed_out() || values.0, "等待 stop 进入超时");
            }
        }

        fn release(&self) {
            let (state, changed) = &*self.state;
            state.lock().expect("停止闸锁不应中毒").1 = true;
            changed.notify_all();
        }
    }

    struct BlockingStopProcess {
        gate: StopGate,
    }

    impl RuntimeProcess for BlockingStopProcess {
        fn id(&self) -> u32 {
            84
        }

        fn try_wait(&mut self) -> Result<Option<ExitStatus>, RuntimeError> {
            Ok(None)
        }

        fn stop(&mut self, _grace: Duration) -> Result<StopOutcome, RuntimeError> {
            let (state, changed) = &*self.gate.state;
            let mut values = state.lock().expect("停止闸锁不应中毒");
            values.0 = true;
            changed.notify_all();
            while !values.1 {
                values = changed.wait(values).expect("停止闸锁不应中毒");
            }
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

    struct BlockingStopLauncher {
        gate: StopGate,
    }

    impl ProcessLauncher for BlockingStopLauncher {
        fn spawn(
            &self,
            _spec: &RuntimeLaunchSpec,
        ) -> Result<Box<dyn RuntimeProcess>, RuntimeError> {
            Ok(Box::new(BlockingStopProcess {
                gate: self.gate.clone(),
            }))
        }
    }

    impl ProcessLauncher for FakeLauncher {
        fn spawn(
            &self,
            _spec: &RuntimeLaunchSpec,
        ) -> Result<Box<dyn RuntimeProcess>, RuntimeError> {
            Ok(Box::new(FakeProcess {
                state: self.state.clone(),
            }))
        }
    }

    struct FakeProbe {
        result: Result<String, RuntimeError>,
    }

    #[derive(Clone, Default)]
    struct ProbeGate {
        state: Arc<(Mutex<bool>, Condvar)>,
    }

    impl ProbeGate {
        fn wait_until_entered(&self, timeout: Duration) {
            let deadline = Instant::now() + timeout;
            let (entered, changed) = &*self.state;
            let mut value = entered.lock().expect("探活闸锁不应中毒");
            while !*value {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "等待后台探活超时");
                let (next, result) = changed
                    .wait_timeout(value, remaining)
                    .expect("探活闸锁不应中毒");
                value = next;
                assert!(!result.timed_out() || *value, "等待后台探活超时");
            }
        }
    }

    struct BlockingProbe {
        entered: ProbeGate,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    struct GateStartupAbort {
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl StartupAbort for GateStartupAbort {
        fn abort(&self) -> Result<(), RuntimeError> {
            let (released, changed) = &*self.release;
            *released.lock().expect("abort release") = true;
            changed.notify_all();
            Ok(())
        }
    }

    struct AbortableStartingProcess {
        release: Arc<(Mutex<bool>, Condvar)>,
        stop_calls: Arc<Mutex<u32>>,
    }

    impl RuntimeProcess for AbortableStartingProcess {
        fn id(&self) -> u32 {
            91
        }

        fn try_wait(&mut self) -> Result<Option<ExitStatus>, RuntimeError> {
            Ok(None)
        }

        fn stop(&mut self, _grace: Duration) -> Result<StopOutcome, RuntimeError> {
            *self.stop_calls.lock().expect("stop calls") += 1;
            Ok(StopOutcome::Terminated)
        }

        fn wait_for_readiness(
            &mut self,
            port: u16,
            _timeout: Duration,
        ) -> Result<ReadinessSignal, RuntimeError> {
            Ok(ReadinessSignal::Web { port })
        }

        fn startup_abort(&self) -> Option<Arc<dyn StartupAbort>> {
            Some(Arc::new(GateStartupAbort {
                release: Arc::clone(&self.release),
            }))
        }
    }

    struct AbortableStartingLauncher {
        release: Arc<(Mutex<bool>, Condvar)>,
        stop_calls: Arc<Mutex<u32>>,
    }

    impl ProcessLauncher for AbortableStartingLauncher {
        fn spawn(
            &self,
            _spec: &RuntimeLaunchSpec,
        ) -> Result<Box<dyn RuntimeProcess>, RuntimeError> {
            Ok(Box::new(AbortableStartingProcess {
                release: Arc::clone(&self.release),
                stop_calls: Arc::clone(&self.stop_calls),
            }))
        }
    }

    impl ReadyProbe for BlockingProbe {
        fn wait_until_ready(&self, _port: u16, _timeout: Duration) -> Result<String, RuntimeError> {
            let (entered, changed) = &*self.entered.state;
            *entered.lock().expect("探活闸锁不应中毒") = true;
            changed.notify_all();
            let (released, released_changed) = &*self.release;
            let mut value = released.lock().expect("探活释放锁不应中毒");
            while !*value {
                value = released_changed.wait(value).expect("探活释放锁不应中毒");
            }
            Ok("http://127.0.0.1:43127".to_owned())
        }
    }

    impl ReadyProbe for FakeProbe {
        fn wait_until_ready(&self, _port: u16, _timeout: Duration) -> Result<String, RuntimeError> {
            match &self.result {
                Ok(url) => Ok(url.clone()),
                Err(RuntimeError::HealthTimeout { port, timeout_ms }) => {
                    Err(RuntimeError::HealthTimeout {
                        port: *port,
                        timeout_ms: *timeout_ms,
                    })
                }
                Err(error) => panic!("测试探活错误不可克隆: {error}"),
            }
        }
    }

    fn test_spec() -> RuntimeLaunchSpec {
        RuntimeLaunchSpec::mock(
            PathBuf::from("node.exe"),
            PathBuf::from("mock-dsh.mjs"),
            PathBuf::from("dsh-home"),
            43127,
        )
    }

    fn wait_until_running(supervisor: &RuntimeSupervisor, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if matches!(
                *supervisor.state.lock().expect("状态锁不应中毒"),
                SupervisorState::Running { .. }
            ) {
                return;
            }
            assert!(Instant::now() < deadline, "等待运行态超时");
            thread::yield_now();
        }
    }

    #[test]
    fn successful_start_returns_before_background_probe_and_emits_ordered_events() {
        let process_state = FakeProcessState::default();
        let sink = RecordingSink::default();
        let supervisor = RuntimeSupervisor::for_test(
            Arc::new(FakeLauncher {
                state: process_state,
            }),
            Arc::new(FakeProbe {
                result: Ok("http://127.0.0.1:43127".to_owned()),
            }),
        );

        supervisor
            .start(test_spec(), Duration::from_secs(1), Arc::new(sink.clone()))
            .expect("调度后台启动应立即成功");

        assert_eq!(
            sink.wait_for_count(2, Duration::from_secs(1)),
            vec![
                RuntimeEvent::Starting {
                    message: "正在启动 DSH".to_owned(),
                },
                RuntimeEvent::Ready {
                    url: "http://127.0.0.1:43127".to_owned(),
                    elapsed_ms: 0,
                },
            ]
        );
    }

    #[test]
    fn official_policy_requires_stdout_readiness_before_http_success() {
        let process_state = FakeProcessState::default();
        let sink = RecordingSink::default();
        let supervisor = RuntimeSupervisor::for_test(
            Arc::new(ReadinessFailLauncher {
                state: process_state.clone(),
            }),
            Arc::new(FakeProbe {
                result: Ok("http://127.0.0.1:43127".to_owned()),
            }),
        );
        let mut spec = test_spec();
        spec.readiness_policy = ReadinessPolicy::StdoutAndHttp;

        supervisor
            .start(spec, Duration::from_secs(1), Arc::new(sink.clone()))
            .expect("后台启动应成功调度");

        let events = sink.wait_for_count(2, Duration::from_secs(1));
        assert!(matches!(events[0], RuntimeEvent::Starting { .. }));
        assert!(matches!(
            &events[1],
            RuntimeEvent::Failed { code, .. } if code == "output_readiness_timeout"
        ));
        assert_eq!(
            *process_state.stop_calls.lock().expect("停止计数锁不应中毒"),
            1
        );
    }

    #[test]
    fn mock_policy_keeps_http_only_readiness() {
        let sink = RecordingSink::default();
        let supervisor = RuntimeSupervisor::for_test(
            Arc::new(ReadinessFailLauncher {
                state: FakeProcessState::default(),
            }),
            Arc::new(FakeProbe {
                result: Ok("http://127.0.0.1:43127".to_owned()),
            }),
        );

        supervisor
            .start(test_spec(), Duration::from_secs(1), Arc::new(sink.clone()))
            .expect("后台启动应成功调度");

        assert!(matches!(
            sink.wait_for_count(2, Duration::from_secs(1))[1],
            RuntimeEvent::Ready { .. }
        ));
    }

    #[test]
    fn failed_probe_stops_child_once_and_emits_failure_once() {
        let process_state = FakeProcessState::default();
        let sink = RecordingSink::default();
        let supervisor = RuntimeSupervisor::for_test(
            Arc::new(FakeLauncher {
                state: process_state.clone(),
            }),
            Arc::new(FakeProbe {
                result: Err(RuntimeError::HealthTimeout {
                    port: 43127,
                    timeout_ms: 1,
                }),
            }),
        );

        supervisor
            .start(
                test_spec(),
                Duration::from_millis(1),
                Arc::new(sink.clone()),
            )
            .expect("调度后台启动应立即成功");

        let events = sink.wait_for_count(2, Duration::from_secs(1));
        assert!(matches!(events[0], RuntimeEvent::Starting { .. }));
        assert!(matches!(events[1], RuntimeEvent::Failed { .. }));
        assert_eq!(
            process_state
                .stop_calls
                .lock()
                .expect("停止计数锁不应中毒")
                .to_owned(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::Failed { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn blocked_probe_does_not_block_start_and_duplicate_start_is_atomic() {
        let entered = ProbeGate::default();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let sink = RecordingSink::default();
        let supervisor = RuntimeSupervisor::for_test(
            Arc::new(FakeLauncher {
                state: FakeProcessState::default(),
            }),
            Arc::new(BlockingProbe {
                entered: entered.clone(),
                release: Arc::clone(&release),
            }),
        );

        supervisor
            .start(test_spec(), Duration::from_secs(1), Arc::new(sink.clone()))
            .expect("start 不得等待探活");
        entered.wait_until_entered(Duration::from_secs(1));
        assert!(matches!(
            supervisor.start(
                test_spec(),
                Duration::from_secs(1),
                Arc::new(RecordingSink::default())
            ),
            Err(RuntimeError::AlreadyRunning)
        ));

        let (released, changed) = &*release;
        *released.lock().expect("探活释放锁不应中毒") = true;
        changed.notify_all();
        assert!(matches!(
            sink.wait_for_count(2, Duration::from_secs(1))[1],
            RuntimeEvent::Ready { .. }
        ));
    }

    #[test]
    fn ready_sink_failure_reclaims_child_and_emits_failed_once() {
        let process_state = FakeProcessState::default();
        let sink = FailReadySink::default();
        let supervisor = RuntimeSupervisor::for_test(
            Arc::new(FakeLauncher {
                state: process_state.clone(),
            }),
            Arc::new(FakeProbe {
                result: Ok("http://127.0.0.1:43127".to_owned()),
            }),
        );

        supervisor
            .start(test_spec(), Duration::from_secs(1), Arc::new(sink.clone()))
            .expect("后台启动应成功调度");

        let events = sink.events.wait_for_count(3, Duration::from_secs(1));
        assert!(matches!(events[0], RuntimeEvent::Starting { .. }));
        assert!(matches!(events[1], RuntimeEvent::Ready { .. }));
        assert!(matches!(events[2], RuntimeEvent::Failed { .. }));
        assert_eq!(
            *process_state.stop_calls.lock().expect("停止计数锁不应中毒"),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::Failed { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn stopping_sink_failure_still_reclaims_child_and_forms_failed_state() {
        let process_state = FakeProcessState::default();
        let sink = FailStoppingSink::default();
        let supervisor = RuntimeSupervisor::for_test(
            Arc::new(FakeLauncher {
                state: process_state.clone(),
            }),
            Arc::new(FakeProbe {
                result: Ok("http://127.0.0.1:43127".to_owned()),
            }),
        );
        supervisor
            .start(test_spec(), Duration::from_secs(1), Arc::new(sink.clone()))
            .expect("后台启动应成功调度");
        wait_until_running(&supervisor, Duration::from_secs(1));

        assert!(matches!(
            supervisor.stop(Duration::ZERO),
            Err(RuntimeError::Tauri(_))
        ));

        let events = sink.events.wait_for_count(4, Duration::from_secs(1));
        assert!(matches!(events[2], RuntimeEvent::Stopping { .. }));
        assert!(matches!(events[3], RuntimeEvent::Failed { .. }));
        assert_eq!(
            *process_state.stop_calls.lock().expect("停止计数锁不应中毒"),
            1
        );
        assert!(matches!(
            *supervisor.state.lock().expect("状态锁不应中毒"),
            SupervisorState::Failed
        ));
    }

    #[test]
    fn start_is_rejected_while_previous_child_is_stopping_without_holding_state_lock() {
        let gate = StopGate::default();
        let sink = RecordingSink::default();
        let supervisor = Arc::new(RuntimeSupervisor::for_test(
            Arc::new(BlockingStopLauncher { gate: gate.clone() }),
            Arc::new(FakeProbe {
                result: Ok("http://127.0.0.1:43127".to_owned()),
            }),
        ));
        supervisor
            .start(test_spec(), Duration::from_secs(1), Arc::new(sink))
            .expect("第一次启动应成功调度");
        wait_until_running(&supervisor, Duration::from_secs(1));

        let stopping = Arc::clone(&supervisor);
        let stop_thread = thread::spawn(move || stopping.stop(Duration::ZERO));
        gate.wait_until_entered(Duration::from_secs(1));

        assert!(matches!(
            supervisor.start(
                test_spec(),
                Duration::from_secs(1),
                Arc::new(RecordingSink::default())
            ),
            Err(RuntimeError::AlreadyRunning)
        ));
        gate.release();
        stop_thread
            .join()
            .expect("停止线程不应 panic")
            .expect("停止应成功");
    }

    #[test]
    fn abort_startup_terminates_blocked_starting_process_and_converges_state() {
        let entered = ProbeGate::default();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let stop_calls = Arc::new(Mutex::new(0_u32));
        let supervisor = RuntimeSupervisor::for_test(
            Arc::new(AbortableStartingLauncher {
                release: Arc::clone(&release),
                stop_calls: Arc::clone(&stop_calls),
            }),
            Arc::new(BlockingProbe {
                entered: entered.clone(),
                release,
            }),
        );
        supervisor
            .start(
                test_spec(),
                Duration::from_secs(30),
                Arc::new(RecordingSink::default()),
            )
            .expect("start scheduled");
        entered.wait_until_entered(Duration::from_secs(1));

        supervisor
            .abort_startup(Duration::from_secs(1))
            .expect("abort starting");

        assert!(supervisor.is_inactive().expect("inactive"));
        assert_eq!(*stop_calls.lock().expect("stop calls"), 1);
        assert!(matches!(
            *supervisor.state.lock().expect("state"),
            SupervisorState::Failed
        ));
    }
}
