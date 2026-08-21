use super::RuntimeError;
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeLaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl RuntimeLaunchSpec {
    /// 构造开发测试用的 Node 模拟运行时命令。
    ///
    /// 参数保持为独立值，确保包含空格或非 ASCII 字符的路径不会经过 shell
    /// 二次解析；固定回环地址避免模拟服务暴露到局域网。
    ///
    /// :param node: Node.js 可执行文件路径。
    /// :param script: 模拟 DSH 服务脚本路径。
    /// :param dsh_home: 隔离的 DSH 用户数据目录。
    /// :param port: 已申请的本地监听端口。
    /// :return: 可直接交给 `std::process::Command` 的启动值对象。
    /// :raises: 此函数只构造值，不产生错误。
    pub fn mock(node: PathBuf, script: PathBuf, dsh_home: PathBuf, port: u16) -> Self {
        let args = vec![
            script.to_string_lossy().into_owned(),
            "--host".to_owned(),
            Ipv4Addr::LOCALHOST.to_string(),
            "--port".to_owned(),
            port.to_string(),
        ];
        let env = BTreeMap::from([
            (
                "DSH_HOME".to_owned(),
                dsh_home.to_string_lossy().into_owned(),
            ),
            ("NO_COLOR".to_owned(), "1".to_owned()),
        ]);

        Self {
            program: node,
            args,
            env,
        }
    }
}

/// 申请一个可供 DSH 随后绑定的动态回环端口。
///
/// 函数仅在 `127.0.0.1` 上让操作系统分配端口，并在返回前释放临时监听器。
/// 这会保留极短的检查与实际启动间竞态，调用方仍需处理 DSH 绑定失败。
///
/// :return: 操作系统分配的非零 TCP 端口。
/// :raises RuntimeError: 无法绑定回环地址或读取本地地址时返回 I/O 错误。
pub fn reserve_loopback_port() -> Result<u16, RuntimeError> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

#[cfg(test)]
mod tests {
    use super::RuntimeLaunchSpec;
    use std::path::PathBuf;

    #[test]
    fn mock_spec_never_uses_shell_command_string() {
        let spec = RuntimeLaunchSpec::mock(
            PathBuf::from(r"C:\Program Files\nodejs\node.exe"),
            PathBuf::from(r"D:\dsh desktop\tests\fixtures\mock-dsh.mjs"),
            PathBuf::from(r"C:\Users\demo\AppData\Roaming\DSH Desktop\dsh-home"),
            43127,
        );

        assert_eq!(
            spec.program,
            PathBuf::from(r"C:\Program Files\nodejs\node.exe")
        );
        assert_eq!(
            spec.args,
            vec![
                r"D:\dsh desktop\tests\fixtures\mock-dsh.mjs",
                "--host",
                "127.0.0.1",
                "--port",
                "43127",
            ]
        );
        assert_eq!(
            spec.env.get("DSH_HOME").unwrap(),
            r"C:\Users\demo\AppData\Roaming\DSH Desktop\dsh-home"
        );
        assert_eq!(spec.env.get("NO_COLOR").unwrap(), "1");
        assert_eq!(spec.env.len(), 2);
    }

    #[test]
    fn reserved_port_is_released_for_a_loopback_server() {
        let port = super::reserve_loopback_port().expect("应能申请动态回环端口");
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .expect("返回前必须释放端口，以便 DSH 立即绑定");

        assert_eq!(
            listener.local_addr().unwrap().ip(),
            std::net::Ipv4Addr::LOCALHOST
        );
        assert_ne!(port, 0);
    }
}
