#![cfg(windows)]

use dsh_desktop_lib::runtime::command::{RuntimeLaunchSpec, reserve_loopback_port};
use dsh_desktop_lib::runtime::process::{ManagedChild, StopOutcome};
use std::collections::BTreeMap;
use std::env;
use std::net::{Ipv4Addr, TcpStream};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::STILL_ACTIVE;
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

// Windows 会短暂复用动态端口；串行覆盖“释放端口 -> Node 绑定”的测试窗口，
// 避免同一测试二进制内的 mock 服务互相抢占端口并掩盖生命周期行为。
static PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

fn process_test_lock() -> MutexGuard<'static, ()> {
    PROCESS_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fixture_spec(port: u16) -> RuntimeLaunchSpec {
    let node = env::var_os("DSH_DESKTOP_NODE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("node.exe"));
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/mock-dsh.mjs");
    let isolated_home = env::temp_dir()
        .join("dsh-desktop-process-tests")
        .join(format!("{}-{port}", std::process::id()));

    RuntimeLaunchSpec::mock(node, script, isolated_home, port)
}

fn immediately_exiting_spec() -> RuntimeLaunchSpec {
    let node = env::var_os("DSH_DESKTOP_NODE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("node.exe"));

    RuntimeLaunchSpec {
        program: node,
        args: vec!["-e".to_owned(), "process.exit(0)".to_owned()],
        env: BTreeMap::new(),
    }
}

fn open_process(pid: u32) -> Option<OwnedHandle> {
    let Ok(raw_handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return None;
    };

    // OpenProcess 将所有权交给调用方；立即包装，确保每次轮询都关闭查询句柄。
    Some(unsafe { OwnedHandle::from_raw_handle(raw_handle.0) })
}

fn process_exists(pid: u32) -> bool {
    open_process(pid).is_some()
}

fn process_is_running(pid: u32) -> bool {
    let Some(handle) = open_process(pid) else {
        return false;
    };
    let mut exit_code = 0_u32;
    unsafe {
        GetExitCodeProcess(
            windows::Win32::Foundation::HANDLE(handle.as_raw_handle()),
            &mut exit_code,
        )
    }
    .is_ok()
        && exit_code == STILL_ACTIVE.0 as u32
}

fn wait_until_ready(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

fn wait_until_process_exits(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_exists(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    !process_exists(pid)
}

fn wait_until_process_stops(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_is_running(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    !process_is_running(pid)
}

#[test]
fn managed_child_starts_a_live_mock_process() {
    let _serial = process_test_lock();
    let port = reserve_loopback_port().expect("应能申请动态回环端口");
    let child = ManagedChild::spawn(&fixture_spec(port)).expect("模拟服务应启动");

    assert!(
        wait_until_ready(port, Duration::from_secs(2)),
        "mock DSH 应在动态端口开始监听"
    );
    assert!(process_is_running(child.id()), "受管子进程应保持存活");
}

#[test]
fn dropping_managed_child_reclaims_the_process() {
    let _serial = process_test_lock();
    let port = reserve_loopback_port().expect("应能申请动态回环端口");
    let child = ManagedChild::spawn(&fixture_spec(port)).expect("模拟服务应启动");
    let pid = child.id();
    assert!(wait_until_ready(port, Duration::from_secs(2)));

    drop(child);

    assert!(
        wait_until_process_exits(pid, Duration::from_secs(2)),
        "关闭 Job handle 后应回收 mock DSH"
    );
}

#[test]
fn stop_reports_exited_when_node_finished_during_grace_period() {
    let _serial = process_test_lock();
    let mut child = ManagedChild::spawn(&immediately_exiting_spec()).expect("Node 应启动");
    let pid = child.id();

    assert_eq!(
        child
            .stop(Duration::from_secs(2))
            .expect("应观察到正常退出"),
        StopOutcome::Exited
    );
    assert!(wait_until_process_stops(pid, Duration::from_secs(2)));
}

#[test]
fn stop_accepts_a_large_grace_period_without_overflowing_the_deadline() {
    let _serial = process_test_lock();
    let mut child = ManagedChild::spawn(&immediately_exiting_spec()).expect("Node 应启动");

    assert_eq!(
        child
            .stop(Duration::MAX)
            .expect("大宽限期不应导致时间计算溢出"),
        StopOutcome::Exited
    );
}

#[test]
fn stop_terminates_a_live_mock_after_the_grace_period() {
    let _serial = process_test_lock();
    let port = reserve_loopback_port().expect("应能申请动态回环端口");
    let mut child = ManagedChild::spawn(&fixture_spec(port)).expect("模拟服务应启动");
    let pid = child.id();
    assert!(wait_until_ready(port, Duration::from_secs(2)));

    assert_eq!(
        child.stop(Duration::ZERO).expect("应强制终止 Job"),
        StopOutcome::Terminated
    );
    assert!(
        wait_until_process_stops(pid, Duration::from_secs(2)),
        "限时停止后应回收 mock DSH"
    );
}
