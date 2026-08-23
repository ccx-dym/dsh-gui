use super::RuntimeError;
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// 定义启动成功需要满足的可信就绪来源。
pub enum ReadinessPolicy {
    /// 阶段 1 mock 仅以 HTTP 根页作为就绪依据。
    HttpOnly,
    /// 官方 DSH 必须同时打印受信任的回环 URL，并通过 HTTP 根页探活。
    StdoutAndHttp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// 不经 shell 解析即可交给进程管理器的完整启动值对象。
pub struct RuntimeLaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub loopback_port: Option<u16>,
    pub readiness_policy: ReadinessPolicy,
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
        let cwd = script
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
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
            cwd,
            loopback_port: Some(port),
            readiness_policy: ReadinessPolicy::HttpOnly,
        }
    }

    /// 构造经过路径边界校验的官方 DSH 启动命令。
    ///
    /// runtime 根只作为安全边界，不会传给子进程；CLI、工作目录和环境变量均以
    /// 独立参数传递，避免空格或非 ASCII 路径被 shell 重新解释。
    ///
    /// :param runtime_root: 用户选中的兼容 runtime 绝对根目录。
    /// :param node: 受控 Node.js 可执行文件绝对路径。
    /// :param cli: runtime 内官方 `lib/bin.js` 的绝对路径。
    /// :param cwd: DSH 的工作区绝对目录。
    /// :param dsh_home: 桌面端隔离数据目录。
    /// :param port: 已申请的本地监听端口。
    /// :return: 固定为回环监听且禁止自动打开浏览器的启动值对象。
    /// :raises RuntimeError: 必需路径不是绝对路径、不存在、类型错误，或 CLI
    ///   规范化后越出所选 runtime 根时返回结构化校验错误。
    pub fn official(
        runtime_root: PathBuf,
        node: PathBuf,
        cli: PathBuf,
        cwd: PathBuf,
        dsh_home: PathBuf,
        port: u16,
    ) -> Result<Self, RuntimeError> {
        if port == 0 {
            return Err(RuntimeError::InvalidLoopbackPort { port });
        }
        validate_directory("runtime_root", &runtime_root)?;
        validate_file("node", &node)?;
        validate_file("cli", &cli)?;
        validate_directory("cwd", &cwd)?;
        validate_directory("dsh_home", &dsh_home)?;

        let canonical_root = runtime_root
            .canonicalize()
            .map_err(|_| invalid_path("runtime_root", "目录无法规范化"))?;
        let canonical_node = node
            .canonicalize()
            .map_err(|_| invalid_path("node", "文件无法规范化"))?;
        let canonical_cli = cli
            .canonicalize()
            .map_err(|_| invalid_path("cli", "文件无法规范化"))?;
        if !canonical_node.starts_with(&canonical_root) {
            return Err(invalid_path("node", "Node 必须位于所选 runtime 内"));
        }
        if !canonical_cli.starts_with(&canonical_root) {
            return Err(invalid_path("cli", "CLI 必须位于所选 runtime 内"));
        }

        let args = vec![
            child_argument_path(&canonical_cli),
            "web".to_owned(),
            "--host".to_owned(),
            Ipv4Addr::LOCALHOST.to_string(),
            "--port".to_owned(),
            port.to_string(),
            "--no-open".to_owned(),
        ];
        let env = BTreeMap::from([
            (
                "DSH_HOME".to_owned(),
                dsh_home.to_string_lossy().into_owned(),
            ),
            ("NO_COLOR".to_owned(), "1".to_owned()),
        ]);

        Ok(Self {
            program: canonical_node,
            args,
            env,
            cwd,
            loopback_port: Some(port),
            readiness_policy: ReadinessPolicy::StdoutAndHttp,
        })
    }
}

fn child_argument_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    #[cfg(windows)]
    {
        // `canonicalize` 会返回 Win32 扩展路径；Node 24 把作为 argv 传入的
        // `\\?\C:\...` 错误拆成 `C:` 目录。CLI 位于固定用户目录且已完成边界
        // 校验，传给子进程前恢复标准盘符/UNC 表示即可保留安全性与兼容性。
        if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = value.strip_prefix(r"\\?\") {
            return rest.to_owned();
        }
    }
    value.into_owned()
}

fn validate_directory(field: &'static str, path: &Path) -> Result<(), RuntimeError> {
    if !path.is_absolute() {
        return Err(invalid_path(field, "路径必须为绝对路径"));
    }
    if !path.is_dir() {
        return Err(invalid_path(field, "目录不存在"));
    }
    Ok(())
}

fn validate_file(field: &'static str, path: &Path) -> Result<(), RuntimeError> {
    if !path.is_absolute() {
        return Err(invalid_path(field, "路径必须为绝对路径"));
    }
    if !path.is_file() {
        return Err(invalid_path(field, "文件不存在"));
    }
    Ok(())
}

fn invalid_path(field: &'static str, reason: &'static str) -> RuntimeError {
    RuntimeError::InvalidLaunchPath { field, reason }
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
    use super::{ReadinessPolicy, RuntimeLaunchSpec, child_argument_path};
    use crate::runtime::RuntimeError;
    use std::fs;
    use std::path::PathBuf;

    struct TestLayout {
        root: PathBuf,
        node: PathBuf,
        cli: PathBuf,
        cwd: PathBuf,
        dsh_home: PathBuf,
    }

    impl TestLayout {
        fn create(label: &str) -> Self {
            // 使用稳定的小型临时布局，重复测试只覆盖同一组空文件，不持续堆积目录。
            let root = std::env::temp_dir()
                .join("dsh-desktop-command-tests")
                .join(format!("中文 空格-{label}"));
            let node = root.join("node").join("node.exe");
            let cli = root
                .join("runtime")
                .join("node_modules")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib")
                .join("bin.js");
            let cwd = root.join("工作 空间");
            let dsh_home = root.join("数据 目录");
            fs::create_dir_all(node.parent().unwrap()).expect("应创建 Node 目录");
            fs::create_dir_all(cli.parent().unwrap()).expect("应创建 CLI 目录");
            fs::create_dir_all(&cwd).expect("应创建工作目录");
            fs::create_dir_all(&dsh_home).expect("应创建数据目录");
            fs::write(&node, []).expect("应创建 Node 文件");
            fs::write(&cli, []).expect("应创建 CLI 文件");
            Self {
                root,
                node,
                cli,
                cwd,
                dsh_home,
            }
        }
    }

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
        assert_eq!(spec.loopback_port, Some(43127));
        assert_eq!(spec.cwd, PathBuf::from(r"D:\dsh desktop\tests\fixtures"));
        assert_eq!(spec.readiness_policy, ReadinessPolicy::HttpOnly);
    }

    #[test]
    #[cfg(windows)]
    fn official_spec_passes_node_a_standard_windows_cli_path() {
        let layout = TestLayout::create("node-standard-path");
        let spec = RuntimeLaunchSpec::official(
            layout.root.clone(),
            layout.node,
            layout.cli,
            layout.cwd,
            layout.dsh_home,
            43127,
        )
        .expect("official spec");

        assert!(!spec.args[0].starts_with(r"\\?\"));
        assert!(spec.args[0].ends_with(r"@deepseek-ai\dsh\lib\bin.js"));
        assert_eq!(
            child_argument_path(std::path::Path::new(r"\\?\UNC\server\share\bin.js")),
            r"\\server\share\bin.js"
        );
    }

    #[test]
    fn official_spec_preserves_unicode_paths_and_fixed_loopback_arguments() {
        let layout = TestLayout::create("official");

        let spec = RuntimeLaunchSpec::official(
            layout.root.clone(),
            layout.node.clone(),
            layout.cli.clone(),
            layout.cwd.clone(),
            layout.dsh_home.clone(),
            43127,
        )
        .expect("有效的官方 runtime 布局应生成启动参数");

        assert_eq!(spec.program, layout.node.canonicalize().unwrap());
        assert_eq!(
            spec.args,
            vec![
                child_argument_path(&layout.cli.canonicalize().unwrap()),
                "web".to_owned(),
                "--host".to_owned(),
                "127.0.0.1".to_owned(),
                "--port".to_owned(),
                "43127".to_owned(),
                "--no-open".to_owned(),
            ]
        );
        assert_eq!(spec.cwd, layout.cwd);
        assert_eq!(
            spec.env.get("DSH_HOME"),
            Some(&layout.dsh_home.to_string_lossy().into_owned())
        );
        assert_eq!(spec.env.get("NO_COLOR").map(String::as_str), Some("1"));
        assert_eq!(spec.loopback_port, Some(43127));
        assert_eq!(spec.readiness_policy, ReadinessPolicy::StdoutAndHttp);
    }

    #[test]
    fn official_spec_rejects_relative_or_missing_launch_paths() {
        let layout = TestLayout::create("invalid");
        let cases = [
            ("runtime_root", PathBuf::from("relative-runtime")),
            ("node", PathBuf::from("node.exe")),
            ("cli", PathBuf::from("lib/bin.js")),
            ("cwd", PathBuf::from("workspace")),
        ];

        for (field, invalid) in cases {
            let mut root = layout.root.clone();
            let mut node = layout.node.clone();
            let mut cli = layout.cli.clone();
            let mut cwd = layout.cwd.clone();
            match field {
                "runtime_root" => root = invalid,
                "node" => node = invalid,
                "cli" => cli = invalid,
                "cwd" => cwd = invalid,
                _ => unreachable!(),
            }
            assert!(matches!(
                RuntimeLaunchSpec::official(
                    root,
                    node,
                    cli,
                    cwd,
                    layout.dsh_home.clone(),
                    43127,
                ),
                Err(RuntimeError::InvalidLaunchPath { field: actual, .. }) if actual == field
            ));
        }

        let missing = layout.root.join("missing-node.exe");
        assert!(matches!(
            RuntimeLaunchSpec::official(
                layout.root.clone(),
                missing,
                layout.cli.clone(),
                layout.cwd.clone(),
                layout.dsh_home.clone(),
                43127,
            ),
            Err(RuntimeError::InvalidLaunchPath { field: "node", .. })
        ));
    }

    #[test]
    fn official_spec_rejects_cli_outside_selected_runtime() {
        let layout = TestLayout::create("outside");
        let outside_root = std::env::temp_dir()
            .join("dsh-desktop-command-tests")
            .join("runtime之外");
        fs::create_dir_all(&outside_root).expect("应创建 runtime 外目录");
        let outside_cli = outside_root.join("bin.js");
        fs::write(&outside_cli, []).expect("应创建 runtime 外 CLI");

        let result = RuntimeLaunchSpec::official(
            layout.root.clone(),
            layout.node.clone(),
            outside_cli,
            layout.cwd.clone(),
            layout.dsh_home.clone(),
            43127,
        );
        assert!(matches!(
            result,
            Err(RuntimeError::InvalidLaunchPath { field: "cli", .. })
        ));
    }

    #[test]
    fn official_spec_rejects_node_outside_selected_runtime() {
        let layout = TestLayout::create("outside-node");
        let outside_node = std::env::temp_dir()
            .join("dsh-desktop-command-tests")
            .join("runtime之外")
            .join("node.exe");
        fs::create_dir_all(outside_node.parent().unwrap()).expect("应创建 runtime 外目录");
        fs::write(&outside_node, []).expect("应创建 runtime 外 Node");

        assert!(matches!(
            RuntimeLaunchSpec::official(
                layout.root.clone(),
                outside_node,
                layout.cli.clone(),
                layout.cwd.clone(),
                layout.dsh_home.clone(),
                43127,
            ),
            Err(RuntimeError::InvalidLaunchPath { field: "node", .. })
        ));
    }

    #[test]
    fn official_spec_requires_an_existing_absolute_dsh_home_directory() {
        let layout = TestLayout::create("invalid-home");
        let home_file = layout.root.join("home-file");
        fs::write(&home_file, []).expect("应创建数据目录反例文件");
        for invalid in [
            PathBuf::from("relative-home"),
            layout.root.join("missing-home"),
            home_file,
        ] {
            assert!(matches!(
                RuntimeLaunchSpec::official(
                    layout.root.clone(),
                    layout.node.clone(),
                    layout.cli.clone(),
                    layout.cwd.clone(),
                    invalid,
                    43127,
                ),
                Err(RuntimeError::InvalidLaunchPath {
                    field: "dsh_home",
                    ..
                })
            ));
        }
    }

    #[test]
    fn official_spec_rejects_zero_loopback_port_with_a_stable_error() {
        let layout = TestLayout::create("zero-port");

        let error = RuntimeLaunchSpec::official(
            layout.root,
            layout.node,
            layout.cli,
            layout.cwd,
            layout.dsh_home,
            0,
        )
        .expect_err("端口 0 不能作为已预留的固定启动端口");

        assert!(matches!(
            error,
            RuntimeError::InvalidLoopbackPort { port: 0 }
        ));
        assert_eq!(error.code(), "invalid_loopback_port");
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
