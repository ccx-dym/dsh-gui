use super::RuntimeError;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(100);
const RETRY_INTERVAL: Duration = Duration::from_millis(50);
const HEALTH_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";

pub trait ReadyProbe: Send + Sync {
    /// 等待指定回环端口返回 HTTP 200。
    ///
    /// :param port: DSH 在 `127.0.0.1` 上监听的端口。
    /// :param timeout: 包含所有连接与重试的总等待时限。
    /// :return: 可交给 WebView 导航的严格回环 URL。
    /// :raises RuntimeError: 总时限内未得到 HTTP 200 时返回探活超时。
    fn wait_until_ready(&self, port: u16, timeout: Duration) -> Result<String, RuntimeError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HealthProbe;

impl ReadyProbe for HealthProbe {
    fn wait_until_ready(&self, port: u16, timeout: Duration) -> Result<String, RuntimeError> {
        let started = Instant::now();
        let deadline = started + timeout;
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));

        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(RuntimeError::HealthTimeout {
                    port,
                    timeout_ms: timeout.as_millis() as u64,
                });
            }

            let attempt_timeout = deadline.saturating_duration_since(now).min(ATTEMPT_TIMEOUT);
            // 启动期间出现连接拒绝、短读、连接重置都属于可恢复状态。单次尝试
            // 的错误被有意丢弃，避免 `?` 绕过总截止时间与后续重试。
            if probe_once(address, attempt_timeout).unwrap_or(false) {
                return Ok(format!("http://127.0.0.1:{port}"));
            }

            let sleep_for = deadline
                .saturating_duration_since(Instant::now())
                .min(RETRY_INTERVAL);
            if !sleep_for.is_zero() {
                thread::sleep(sleep_for);
            }
        }
    }
}

fn probe_once(address: SocketAddr, timeout: Duration) -> std::io::Result<bool> {
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(HEALTH_REQUEST)?;

    let mut response = [0_u8; 64];
    let mut received = 0;
    while received < response.len() {
        let read = stream.read(&mut response[received..])?;
        if read == 0 {
            break;
        }
        received += read;
        if response[..received]
            .windows(2)
            .any(|bytes| bytes == b"\r\n")
        {
            break;
        }
    }
    Ok(is_http_200(&response[..received]))
}

fn is_http_200(response: &[u8]) -> bool {
    let status_line = response
        .split(|byte| *byte == b'\r' || *byte == b'\n')
        .next()
        .unwrap_or_default();

    [b"HTTP/1.1 200".as_slice(), b"HTTP/1.0 200".as_slice()]
        .into_iter()
        .any(|prefix| {
            status_line
                .strip_prefix(prefix)
                .is_some_and(|reason| reason.is_empty() || reason.starts_with(b" "))
        })
}

#[cfg(test)]
mod tests {
    use super::{HealthProbe, ReadyProbe};
    use crate::runtime::RuntimeError;
    use std::io::Write;
    use std::net::{Ipv4Addr, Shutdown, TcpListener};
    use std::thread;
    use std::time::{Duration, Instant};

    const OK_RESPONSE: &[u8] =
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK";

    enum Reply {
        Bytes(&'static [u8]),
        Reset,
    }

    fn response_server(replies: Vec<Reply>) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                match reply {
                    Reply::Bytes(bytes) => stream.write_all(bytes).unwrap(),
                    Reply::Reset => stream.shutdown(Shutdown::Both).unwrap(),
                }
            }
        });
        (port, handle)
    }

    #[test]
    fn ready_response_returns_strict_loopback_url() {
        let (port, server) = response_server(vec![Reply::Bytes(OK_RESPONSE)]);

        let url = HealthProbe
            .wait_until_ready(port, Duration::from_millis(500))
            .expect("HTTP 200 应通过探活");

        assert_eq!(url, format!("http://127.0.0.1:{port}"));
        server.join().unwrap();
    }

    #[test]
    fn http_200_accepts_a_valid_custom_reason_phrase() {
        let (port, server) = response_server(vec![Reply::Bytes(
            b"HTTP/1.0 200 Ready\r\nContent-Length: 0\r\n\r\n",
        )]);

        let url = HealthProbe
            .wait_until_ready(port, Duration::from_millis(500))
            .expect("合法的 200 理由短语不应影响状态码判断");

        assert_eq!(url, format!("http://127.0.0.1:{port}"));
        server.join().unwrap();
    }

    #[test]
    fn connection_refusal_is_retried_until_server_starts() {
        let port = crate::runtime::command::reserve_loopback_port().unwrap();
        let server = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(OK_RESPONSE).unwrap();
        });

        let url = HealthProbe
            .wait_until_ready(port, Duration::from_millis(600))
            .expect("启动期间的连接拒绝必须重试");

        assert_eq!(url, format!("http://127.0.0.1:{port}"));
        server.join().unwrap();
    }

    #[test]
    fn short_status_line_is_retried() {
        let (port, server) =
            response_server(vec![Reply::Bytes(b"HTTP/1."), Reply::Bytes(OK_RESPONSE)]);

        let url = HealthProbe
            .wait_until_ready(port, Duration::from_millis(500))
            .expect("短读必须被视为瞬时失败并重试");

        assert_eq!(url, format!("http://127.0.0.1:{port}"));
        server.join().unwrap();
    }

    #[test]
    fn socket_read_or_write_error_is_retried() {
        let (port, server) = response_server(vec![Reply::Reset, Reply::Bytes(OK_RESPONSE)]);

        let url = HealthProbe
            .wait_until_ready(port, Duration::from_millis(500))
            .expect("连接重置等读写错误不得提前终止总探活");

        assert_eq!(url, format!("http://127.0.0.1:{port}"));
        server.join().unwrap();
    }

    #[test]
    fn non_200_response_is_retried() {
        let (port, server) = response_server(vec![
            Reply::Bytes(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"),
            Reply::Bytes(OK_RESPONSE),
        ]);

        let url = HealthProbe
            .wait_until_ready(port, Duration::from_millis(500))
            .expect("非 200 响应必须重试");

        assert_eq!(url, format!("http://127.0.0.1:{port}"));
        server.join().unwrap();
    }

    #[test]
    fn unavailable_port_times_out_at_total_deadline() {
        let port = crate::runtime::command::reserve_loopback_port().unwrap();
        let started = Instant::now();

        let error = HealthProbe
            .wait_until_ready(port, Duration::from_millis(150))
            .expect_err("未监听端口必须在总截止时间后超时");
        let elapsed = started.elapsed();

        assert!(matches!(
            error,
            RuntimeError::HealthTimeout {
                port: error_port,
                timeout_ms: 150
            } if error_port == port
        ));
        assert!(elapsed >= Duration::from_millis(140));
        assert!(elapsed < Duration::from_millis(500));
    }
}
