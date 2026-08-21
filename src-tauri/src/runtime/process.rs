use super::command::RuntimeLaunchSpec;
use super::{ReadinessSignal, RuntimeError};
use std::ffi::c_void;
use std::io::Read;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::core::{Error as WindowsError, PCWSTR};

/// 受 Windows Job Object 约束的 DSH 子进程。
///
/// Job 句柄是进程树的真正生命周期所有者；将其放入 `OwnedHandle`，避免裸
/// `HANDLE` 被复制后发生重复关闭或泄漏。
pub struct ManagedChild {
    // Option 只用于 Drop 中显式提前关闭，字段内始终保存唯一 OwnedHandle 所有权。
    job: Option<OwnedHandle>,
    child: Child,
    output: RuntimeOutputSink,
}

/// 子进程标准输出/错误输出的有界、脱敏 drain 所有者。
pub struct RuntimeOutputSink {
    readiness: Receiver<ReadinessSignal>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    stderr_observed: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// 不包含原始 stderr 正文的稳定诊断类别。
pub enum RuntimeOutputErrorKind {
    /// stderr 出现过内容；原始字节不会离开 drain 线程。
    StderrObserved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopOutcome {
    /// 子进程在宽限期内自行退出。
    Exited,
    /// 宽限期结束后由 Job Object 终止进程树。
    Terminated,
}

impl ManagedChild {
    /// 启动并把运行时进程绑定到启用整树回收的 Windows Job Object。
    ///
    /// 参数和环境变量逐项传给 `Command`，不会经过 shell 解析。
    ///
    /// :param spec: 已校验的运行时可执行文件、参数和环境变量。
    /// :return: 独占子进程和 Job 句柄的受管进程。
    /// :raises RuntimeError: Job 创建、配置、进程启动或绑定失败时返回；若进程
    ///   已经启动，错误路径会先结束并等待该进程。
    pub fn spawn(spec: &RuntimeLaunchSpec) -> Result<Self, RuntimeError> {
        let raw_job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|error| process_error("CreateJobObjectW", error))?;
        // CreateJobObjectW 成功后把返回句柄的唯一所有权立即转交给 OwnedHandle。
        let job = unsafe { OwnedHandle::from_raw_handle(raw_job.0) };

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                borrowed_handle(&job),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(|error| process_error("SetInformationJobObject", error))?;

        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .envs(&spec.env)
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;

        let assign_result = unsafe {
            AssignProcessToJobObject(borrowed_handle(&job), HANDLE(child.as_raw_handle()))
        };
        if let Err(error) = assign_result {
            // 进程已启动但尚未受 Job 保护，必须在返回错误前同步回收，防止孤儿进程。
            let _ = child.kill();
            let _ = child.wait();
            return Err(process_error("AssignProcessToJobObject", error));
        }

        let stdout = child
            .stdout
            .take()
            .expect("piped stdout 必须存在于刚启动的 Child");
        let stderr = child
            .stderr
            .take()
            .expect("piped stderr 必须存在于刚启动的 Child");
        let (readiness_tx, readiness) = mpsc::sync_channel(1);
        let expected_port = spec.loopback_port;
        let stdout_thread = match thread::Builder::new()
            .name("dsh-runtime-stdout".to_owned())
            .spawn(move || drain_stdout(stdout, readiness_tx, expected_port))
        {
            Ok(handle) => handle,
            Err(error) => {
                drop(job);
                let _ = child.kill();
                let _ = child.wait();
                return Err(RuntimeError::Io(error));
            }
        };
        let stderr_observed = Arc::new(AtomicBool::new(false));
        let worker_stderr_observed = Arc::clone(&stderr_observed);
        let stderr_thread = match thread::Builder::new()
            .name("dsh-runtime-stderr".to_owned())
            .spawn(move || drain_stderr(stderr, worker_stderr_observed))
        {
            Ok(handle) => handle,
            Err(error) => {
                // 先关闭 Job 结束进程树，stdout 才能可靠到达 EOF 并完成 join。
                drop(job);
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                return Err(RuntimeError::Io(error));
            }
        };

        Ok(Self {
            job: Some(job),
            child,
            output: RuntimeOutputSink {
                readiness,
                stdout_thread: Some(stdout_thread),
                stderr_thread: Some(stderr_thread),
                stderr_observed,
            },
        })
    }

    /// 返回受管主进程的 Windows PID。
    ///
    /// :return: `std::process::Child` 分配的进程标识符。
    /// :raises: 此只读操作不产生错误。
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// 非阻塞查询受管主进程是否已经退出。
    ///
    /// :return: 仍运行时为 `None`，退出后为对应的 `ExitStatus`。
    /// :raises RuntimeError: Windows 无法查询子进程状态时返回 I/O 错误。
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, RuntimeError> {
        Ok(self.child.try_wait()?)
    }

    /// 等待 stdout 中与预期端口完全一致的官方回环就绪信号。
    ///
    /// :param port: 当前启动已分配的动态回环端口。
    /// :param timeout: 等待 stdout 信号的最长时间。
    /// :return: 与端口匹配的结构化就绪信号。
    /// :raises RuntimeError: 超时或 stdout 在合法信号前关闭时返回。
    pub fn wait_for_readiness(
        &mut self,
        port: u16,
        timeout: Duration,
    ) -> Result<ReadinessSignal, RuntimeError> {
        self.output.wait_for_readiness(port, timeout)
    }

    /// 返回脱敏后的运行时输出错误类别。
    ///
    /// :return: stderr 尚无内容时为 `None`，否则只返回稳定类别。
    /// :raises: 此原子只读操作不产生错误，且绝不返回原始 stderr。
    pub fn output_error_kind(&self) -> Option<RuntimeOutputErrorKind> {
        self.output.error_kind()
    }

    /// 在宽限期内观察自然退出，超时后终止整个 Job 进程树。
    ///
    /// Windows 没有适用于任意控制台/GUI 子进程的通用 SIGTERM。本阶段保留
    /// 宽限窗口作为正常停止 seam；后续可在窗口内接入 DSH 自身的关闭协议。
    ///
    /// :param grace: 强制终止前等待子进程自然退出的最长时间。
    /// :return: `Exited` 表示宽限期内退出，`Terminated` 表示终止了整个 Job。
    /// :raises RuntimeError: 查询进程状态或调用 `TerminateJobObject` 失败时返回。
    pub fn stop(&mut self, grace: Duration) -> Result<StopOutcome, RuntimeError> {
        let started_at = Instant::now();
        loop {
            if self.child.try_wait()?.is_some() {
                // 主进程退出不代表后代已退出；先关闭 Job，再 join 可能仍被继承的 pipe。
                drop(self.job.take());
                self.output.join();
                return Ok(StopOutcome::Exited);
            }

            // 以已用时间做饱和减法，避免 Duration::MAX 等合法输入让 Instant 加法溢出。
            let remaining = grace.saturating_sub(started_at.elapsed());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(remaining.min(Duration::from_millis(20)));
        }

        let job = self
            .job
            .as_ref()
            .expect("ManagedChild 存活期间必须持有 Job OwnedHandle");
        unsafe { TerminateJobObject(borrowed_handle(job), 1) }
            .map_err(|error| process_error("TerminateJobObject", error))?;
        // TerminateJobObject 是异步的；等待主子进程句柄发出信号后再向调用方报告完成。
        self.child.wait()?;
        self.output.join();
        Ok(StopOutcome::Terminated)
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        // 必须先关闭 Job：KILL_ON_JOB_CLOSE 由内核结束完整进程树，而不只是主 PID。
        drop(self.job.take());

        // kill 是 Job 关闭失败或退出竞态下的兜底；wait 同时释放 Child 的进程资源。
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        self.output.join();
    }
}

impl RuntimeOutputSink {
    /// 查询 stderr 的脱敏类别，不读取或复制用户正文。
    ///
    /// :return: 是否观察到 stderr 内容的稳定类别。
    /// :raises: 此原子只读操作不产生错误。
    pub fn error_kind(&self) -> Option<RuntimeOutputErrorKind> {
        self.stderr_observed
            .load(Ordering::Relaxed)
            .then_some(RuntimeOutputErrorKind::StderrObserved)
    }

    /// 等待并校验结构化 stdout 就绪事件，不保留原始输出正文。
    ///
    /// :param port: 只接受此动态端口对应的事件。
    /// :param timeout: 最长阻塞时间。
    /// :return: 精确匹配的回环 Web 就绪信号。
    /// :raises RuntimeError: 信号超时或 stdout 已关闭时返回。
    pub fn wait_for_readiness(
        &self,
        port: u16,
        timeout: Duration,
    ) -> Result<ReadinessSignal, RuntimeError> {
        let started_at = Instant::now();
        loop {
            let remaining = timeout.saturating_sub(started_at.elapsed());
            if remaining.is_zero() {
                return Err(RuntimeError::OutputReadinessTimeout {
                    port,
                    timeout_ms: timeout.as_millis() as u64,
                });
            }
            // 限制单次等待可同时规避 Duration::MAX 的平台换算溢出，并及时观察退出。
            match self
                .readiness
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(signal @ ReadinessSignal::Web { port: actual }) if actual == port => {
                    return Ok(signal);
                }
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => {
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(RuntimeError::ProcessExitedEarly);
                }
            }
        }
    }

    fn join(&mut self) {
        if let Some(handle) = self.stdout_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
        // stderr 仅保留“曾出现错误输出”这一类别，不持久化可能包含用户正文的内容。
        let _stderr_category_observed = self.error_kind();
    }
}

fn drain_stdout(
    mut stdout: ChildStdout,
    readiness: mpsc::SyncSender<ReadinessSignal>,
    expected_port: Option<u16>,
) {
    const READ_BUFFER_BYTES: usize = 8 * 1024;
    const MAX_CONTROL_LINE_BYTES: usize = 256;
    let mut read_buffer = [0_u8; READ_BUFFER_BYTES];
    let mut line = Vec::with_capacity(MAX_CONTROL_LINE_BYTES);
    let mut oversized = false;
    while let Ok(count) = stdout.read(&mut read_buffer) {
        if count == 0 {
            break;
        }
        for byte in &read_buffer[..count] {
            if *byte == b'\n' {
                if !oversized {
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    if let Some(signal @ ReadinessSignal::Web { port }) =
                        parse_readiness_line(&line)
                        && Some(port) == expected_port
                    {
                        let _ = readiness.try_send(signal);
                    }
                }
                line.clear();
                oversized = false;
            } else if !oversized {
                if line.len() < MAX_CONTROL_LINE_BYTES {
                    line.push(*byte);
                } else {
                    // 超长正文只 drain，不缓存；直到换行才重新尝试控制行解析。
                    line.clear();
                    oversized = true;
                }
            }
        }
    }
}

fn drain_stderr(mut stderr: ChildStderr, observed: Arc<AtomicBool>) {
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) => break,
            Ok(_) => {
                observed.store(true, Ordering::Relaxed);
            }
            Err(_) => break,
        }
    }
}

fn parse_readiness_line(line: &[u8]) -> Option<ReadinessSignal> {
    let text = std::str::from_utf8(line).ok()?;
    let port_text = text.strip_prefix("dsh web: http://127.0.0.1:")?;
    if port_text.is_empty() || !port_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let port = port_text.parse::<u16>().ok()?;
    if port == 0 || port_text != port.to_string() {
        return None;
    }
    Some(ReadinessSignal::Web { port })
}

fn borrowed_handle(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

fn process_error(operation: &'static str, error: WindowsError) -> RuntimeError {
    RuntimeError::Process {
        operation,
        code: error.code().0,
    }
}
