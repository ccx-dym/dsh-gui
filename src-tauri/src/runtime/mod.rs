pub mod command;
pub mod health;
#[cfg(windows)]
pub mod process;

use std::io;

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
}
