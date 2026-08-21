use super::RuntimeError;
use super::command::RuntimeLaunchSpec;
use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
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

        Ok(Self {
            job: Some(job),
            child,
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
    }
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
