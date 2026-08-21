#![cfg(windows)]

use dsh_desktop_lib::runtime::ReadinessSignal;
use dsh_desktop_lib::runtime::command::{
    ReadinessPolicy, RuntimeLaunchSpec, reserve_loopback_port,
};
use dsh_desktop_lib::runtime::process::{ManagedChild, RuntimeOutputErrorKind, StopOutcome};
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
        cwd: env::current_dir().expect("应读取测试工作目录"),
        loopback_port: None,
        readiness_policy: ReadinessPolicy::HttpOnly,
    }
}

fn node_eval_spec(source: &str, port: u16) -> RuntimeLaunchSpec {
    let node = env::var_os("DSH_DESKTOP_NODE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("node.exe"));
    RuntimeLaunchSpec {
        program: node,
        args: vec!["-e".to_owned(), source.to_owned()],
        env: BTreeMap::new(),
        cwd: env::current_dir().expect("应读取测试工作目录"),
        loopback_port: Some(port),
        readiness_policy: ReadinessPolicy::StdoutAndHttp,
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
    let spec = immediately_exiting_spec();
    assert_eq!(spec.loopback_port, None, "非网络进程不应触发 HTTP 探活");
    let mut child = ManagedChild::spawn(&spec).expect("Node 应启动");
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

#[test]
fn output_drain_accepts_only_the_exact_loopback_readiness_line_after_large_output() {
    let _serial = process_test_lock();
    let port = reserve_loopback_port().expect("应能申请动态回环端口");
    let source = format!(
        "process.stdout.write('x'.repeat(2*1024*1024)+'\\n');\
         process.stderr.write('private'.repeat(256*1024)+'\\n');\
         console.log('dsh web: http://127.0.0.1:{}');\
         console.log('dsh web: http://127.0.0.1:{port}');\
         setInterval(()=>{{}},1000);",
        port.saturating_add(1)
    );
    let mut child = ManagedChild::spawn(&node_eval_spec(&source, port)).expect("Node 应启动");
    thread::sleep(Duration::from_millis(200));

    assert_eq!(
        child
            .wait_for_readiness(port, Duration::from_secs(5))
            .expect("大量输出必须被持续 drain，且不应阻塞精确就绪信号"),
        ReadinessSignal::Web { port }
    );
    let stderr_deadline = Instant::now() + Duration::from_secs(1);
    while child.output_error_kind().is_none() && Instant::now() < stderr_deadline {
        thread::yield_now();
    }
    assert_eq!(
        child.output_error_kind(),
        Some(RuntimeOutputErrorKind::StderrObserved),
        "stderr 只能暴露脱敏类别，不能返回 private 原文"
    );
    child.stop(Duration::ZERO).expect("应停止大输出进程");
}

#[test]
fn output_drain_rejects_non_loopback_and_wrong_port_readiness_lines() {
    let _serial = process_test_lock();
    let port = reserve_loopback_port().expect("应能申请动态回环端口");
    for forged in [
        format!("dsh web: http://0.0.0.0:{port}"),
        format!("dsh web: http://127.0.0.1:{}", port.saturating_add(1)),
        format!("dsh web: http://127.0.0.1:0{port}"),
    ] {
        let source = format!("console.log({forged:?});");
        let mut child = ManagedChild::spawn(&node_eval_spec(&source, port)).expect("Node 应启动");

        assert!(
            child
                .wait_for_readiness(port, Duration::from_secs(1))
                .is_err(),
            "伪造输出不得满足官方 stdout readiness: {forged}"
        );
        child.stop(Duration::ZERO).expect("退出进程应可回收");
    }
}

#[test]
fn output_readiness_accepts_a_large_timeout_without_deadline_overflow() {
    let _serial = process_test_lock();
    let port = reserve_loopback_port().expect("应能申请动态回环端口");
    let source =
        format!("console.log('dsh web: http://127.0.0.1:{port}');setInterval(()=>{{}},1000);");
    let mut child = ManagedChild::spawn(&node_eval_spec(&source, port)).expect("Node 应启动");

    assert_eq!(
        child
            .wait_for_readiness(port, Duration::MAX)
            .expect("合法的大超时不得让截止时间计算溢出"),
        ReadinessSignal::Web { port }
    );
    child.stop(Duration::ZERO).expect("应停止就绪测试进程");
}

#[test]
fn natural_main_exit_terminates_pipe_inheriting_descendants_before_join() {
    let _serial = process_test_lock();
    let port = reserve_loopback_port().expect("应能申请动态回环端口");
    let source = "require('node:child_process').spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:['ignore','inherit','inherit']}).unref();";
    let mut child = ManagedChild::spawn(&node_eval_spec(source, port)).expect("Node 应启动");
    let started = Instant::now();

    assert_eq!(
        child
            .stop(Duration::from_secs(2))
            .expect("主进程自然退出时必须回收继承 pipe 的后代"),
        StopOutcome::Exited
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "join 不得被后代持有的 pipe 卡住"
    );
}
