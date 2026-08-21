use crate::paths::RuntimeLayout;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

const DEPLOYMENT_SCHEMA: u32 = 1;

/// 已安装且可被激活的固定 DSH 运行时版本。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledRuntime {
    pub version: Version,
    pub manifest_digest: String,
    pub node_version: Version,
}

impl InstalledRuntime {
    /// 解析严格 semver 运行时版本。
    ///
    /// :param version: 不带 `v` 前缀的完整 semver 字符串。
    /// :param manifest_digest: 已验证兼容清单的 SHA-256 摘要。
    /// :param node_version: 离线启动所需的严格 Node semver。
    /// :return: 类型化且可离线恢复的已安装运行时标识。
    /// :raises InstallStateError: 版本或摘要无效时返回对应错误。
    pub fn new(
        version: &str,
        manifest_digest: String,
        node_version: &str,
    ) -> Result<Self, InstallStateError> {
        let parsed =
            Version::parse(version).map_err(|source| InstallStateError::InvalidVersion {
                version: version.to_owned(),
                source,
            })?;
        let digest_is_canonical = manifest_digest.len() == 64
            && manifest_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
        if !digest_is_canonical {
            return Err(InstallStateError::InvalidManifestDigest);
        }
        let node_version = Version::parse(node_version).map_err(|source| {
            InstallStateError::InvalidNodeVersion {
                version: node_version.to_owned(),
                source,
            }
        })?;
        Ok(Self {
            version: parsed,
            manifest_digest,
            node_version,
        })
    }

    /// 创建包含离线启动所需 Node 版本的完整运行时 descriptor。
    ///
    /// :param version: DSH 严格 semver。
    /// :param manifest_digest: 已验证兼容清单摘要。
    /// :param node_version: 签名清单固定的 Node 严格 semver。
    /// :return: 可持久化并在崩溃恢复时独立启动的 descriptor。
    /// :raises InstallStateError: 任一版本或摘要无效时返回。
    pub fn with_node_version(
        version: &str,
        manifest_digest: String,
        node_version: &str,
    ) -> Result<Self, InstallStateError> {
        Self::new(version, manifest_digest, node_version)
    }
}

/// 与运行时成对激活的隔离 DSH 数据 generation。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataGeneration {
    pub id: String,
}

impl DataGeneration {
    /// 校验并创建单段 generation 标识。
    ///
    /// :param id: 仅含 ASCII 字母、数字、点、下划线或连字符，且不占用内部
    ///   `.state`/`.dsh-internal-*` 命名空间的目录名。
    /// :return: 不可能逃逸 generation 根目录的数据标识。
    /// :raises InstallStateError: 标识为空、是保留路径段或含其他字符时返回
    ///   `InvalidGeneration`。
    pub fn new(id: &str) -> Result<Self, InstallStateError> {
        let windows_stem = id.split('.').next().unwrap_or_default();
        let reserved_device = matches!(
            windows_stem.to_ascii_uppercase().as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        );
        let valid = !id.is_empty()
            && id != "."
            && id != ".."
            && !id.ends_with('.')
            && !id.eq_ignore_ascii_case(".state")
            && !id.to_ascii_lowercase().starts_with(".dsh-internal-")
            && !reserved_device
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !valid {
            return Err(InstallStateError::InvalidGeneration { id: id.to_owned() });
        }
        Ok(Self { id: id.to_owned() })
    }
}

/// 一次提交的运行时与数据 generation 激活配对。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveDeployment {
    pub runtime: InstalledRuntime,
    pub data: DataGeneration,
    pub activated_at: String,
    pub project_workspace: Option<PathBuf>,
}

impl ActiveDeployment {
    /// 创建待持久化的激活配对。
    ///
    /// :param runtime: 已验证并安装的固定运行时。
    /// :param data: 与运行时配套的数据 generation。
    /// :param activated_at: 发布流程提供的 UTC 激活时间。
    /// :return: 可由 `InstallStateStore` 一次提交的部署值。
    /// :raises: 此构造函数不访问文件系统，不产生错误。
    pub fn new(runtime: InstalledRuntime, data: DataGeneration, activated_at: String) -> Self {
        Self {
            runtime,
            data,
            activated_at,
            project_workspace: None,
        }
    }

    /// 创建包含可信本地项目目录的完整激活配对。
    ///
    /// :param runtime: 含 Node 版本的完整运行时 descriptor。
    /// :param data: 与运行时成对切换的数据 generation。
    /// :param activated_at: UTC 激活时间。
    /// :param project_workspace: 首次设置后规范化的本地项目目录。
    /// :return: 可供 `InstallStateStore` 校验并持久化的完整配对。
    /// :raises: 此值构造器不执行 I/O；存储时会重验目录。
    pub fn with_project_workspace(
        runtime: InstalledRuntime,
        data: DataGeneration,
        activated_at: String,
        project_workspace: PathBuf,
    ) -> Self {
        Self {
            runtime,
            data,
            activated_at,
            project_workspace: Some(project_workspace),
        }
    }
}

/// 安装状态读取、校验和原子提交失败。
#[derive(Debug, Error)]
pub enum InstallStateError {
    #[error("尚未安装兼容运行时")]
    NotInstalled,
    #[error("无效的运行时版本 {version}: {source}")]
    InvalidVersion {
        version: String,
        source: semver::Error,
    },
    #[error("兼容清单摘要不是规范的小写 SHA-256 hex")]
    InvalidManifestDigest,
    #[error("无效的 Node 版本 {version}: {source}")]
    InvalidNodeVersion {
        version: String,
        source: semver::Error,
    },
    #[error("deployment 缺少离线启动 descriptor 字段 {field}")]
    MissingDescriptor { field: &'static str },
    #[error("项目工作目录不是可信的本地普通目录")]
    InvalidProjectWorkspace,
    #[error("无效的数据 generation 标识: {id}")]
    InvalidGeneration { id: String },
    #[error("deployment.json 不是完整有效的 JSON: {source}")]
    InvalidJson { source: serde_json::Error },
    #[error("不支持的 deployment schema: {schema}")]
    UnknownSchema { schema: u32 },
    #[error("不支持的 deployment 状态")]
    UnknownStatus,
    #[error("deployment 字段 {field} 逃逸固定根目录")]
    PathEscape { field: &'static str },
    #[error("待激活的 {target} 目录不存在")]
    DeploymentTargetMissing { target: &'static str },
    #[error("待激活的 {target} 路径不是目录")]
    DeploymentTargetNotDirectory { target: &'static str },
    #[error("安装状态 I/O 失败（{operation} {path}）: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("无法序列化安装状态: {source}")]
    Serialize { source: serde_json::Error },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeploymentDocument {
    schema: u32,
    runtime: RuntimeDocument,
    data: DataDocument,
    activated_at: String,
    project_workspace: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeDocument {
    version: String,
    relative_dir: String,
    manifest_digest: String,
    node_version: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DataDocument {
    id: String,
    relative_dir: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UninstalledDocument {
    schema: u32,
    status: String,
    changed_at: String,
}

/// 在漫游设置目录维护单一原子 deployment 指针。
pub struct InstallStateStore {
    layout: RuntimeLayout,
}

impl InstallStateStore {
    /// 创建与固定目录布局绑定的安装状态存储。
    ///
    /// :param layout: runtime、generation 和 deployment 文件布局。
    /// :return: 不执行 I/O 的状态存储对象。
    /// :raises: 此构造函数不产生错误。
    pub fn new(layout: RuntimeLayout) -> Self {
        Self { layout }
    }

    /// 读取并校验当前激活的 runtime/data 配对。
    ///
    /// :return: schema、相对目录和类型化标识均有效的激活部署。
    /// :raises InstallStateError: 文件缺失、JSON 截断、schema 未知、semver/generation
    ///   无效或相对路径逃逸时返回对应类型化错误。
    pub fn load(&self) -> Result<ActiveDeployment, InstallStateError> {
        let path = self.layout.deployment_file();
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(InstallStateError::NotInstalled);
            }
            Err(source) => return Err(io_error("read", path, source)),
        };
        let raw: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|source| InstallStateError::InvalidJson { source })?;
        if raw.get("status").is_some() {
            let document: UninstalledDocument = serde_json::from_value(raw)
                .map_err(|source| InstallStateError::InvalidJson { source })?;
            if document.schema != DEPLOYMENT_SCHEMA {
                return Err(InstallStateError::UnknownSchema {
                    schema: document.schema,
                });
            }
            if document.status == "uninstalled" {
                return Err(InstallStateError::NotInstalled);
            }
            return Err(InstallStateError::UnknownStatus);
        }
        if let Some(schema) = raw.get("schema").and_then(serde_json::Value::as_u64)
            && schema != u64::from(DEPLOYMENT_SCHEMA)
        {
            return Err(InstallStateError::UnknownSchema {
                schema: u32::try_from(schema).unwrap_or(u32::MAX),
            });
        }
        let document: DeploymentDocument = serde_json::from_value(raw)
            .map_err(|source| InstallStateError::InvalidJson { source })?;
        if document.schema != DEPLOYMENT_SCHEMA {
            return Err(InstallStateError::UnknownSchema {
                schema: document.schema,
            });
        }

        let runtime = InstalledRuntime::with_node_version(
            &document.runtime.version,
            document.runtime.manifest_digest,
            &document.runtime.node_version,
        )?;
        let data = DataGeneration::new(&document.data.id)?;
        let project_workspace = validate_project_workspace(Path::new(&document.project_workspace))?;
        validate_relative_dir(
            "runtime",
            &document.runtime.relative_dir,
            Path::new("dsh").join(runtime.version.to_string()),
        )?;
        validate_relative_dir(
            "data",
            &document.data.relative_dir,
            Path::new("generations").join(&data.id),
        )?;
        Ok(ActiveDeployment::with_project_workspace(
            runtime,
            data,
            document.activated_at,
            project_workspace,
        ))
    }

    /// 将 runtime 与 data 配对写入同一个原子激活指针。
    ///
    /// 先验证安装器已经准备好两个完整目录，再在 deployment 文件同目录写入并 flush
    /// 临时文件；最后一次 rename 才让新配对对读取方可见，因此不会观察到 runtime/data
    /// 各自更新的中间态。此处绝不创建内容目录，避免把缺失安装误激活为空目录。
    ///
    /// :param deployment: 要一次激活的运行时与 generation 配对。
    /// :return: 目录准备与指针提交完成时返回 `Ok(())`。
    /// :raises InstallStateError: 目标目录缺失/不是目录，或创建设置目录、序列化、写入、
    ///   flush、rename 失败时返回。
    pub fn save(&self, deployment: &ActiveDeployment) -> Result<(), InstallStateError> {
        let node_version = &deployment.runtime.node_version;
        let project_workspace = deployment.project_workspace.as_deref().ok_or(
            InstallStateError::MissingDescriptor {
                field: "project_workspace",
            },
        )?;
        let project_workspace = validate_project_workspace(project_workspace)?;
        let runtime_dir = self.layout.runtime_dir(&deployment.runtime);
        let generation_dir = self.layout.generation_dir(&deployment.data);
        validate_deployment_directory("runtime", &runtime_dir)?;
        validate_deployment_directory("generation", &generation_dir)?;

        let destination = self.layout.deployment_file();
        let parent = destination.parent().ok_or(InstallStateError::PathEscape {
            field: "deployment",
        })?;
        fs::create_dir_all(parent).map_err(|source| io_error("create_dir_all", parent, source))?;
        let temporary = parent.join("deployment.json.tmp");
        let document = DeploymentDocument {
            schema: DEPLOYMENT_SCHEMA,
            runtime: RuntimeDocument {
                version: deployment.runtime.version.to_string(),
                relative_dir: format!("dsh/{}", deployment.runtime.version),
                manifest_digest: deployment.runtime.manifest_digest.clone(),
                node_version: node_version.to_string(),
            },
            data: DataDocument {
                id: deployment.data.id.clone(),
                relative_dir: format!("generations/{}", deployment.data.id),
            },
            activated_at: deployment.activated_at.clone(),
            project_workspace: project_workspace.to_string_lossy().into_owned(),
        };
        let bytes = serde_json::to_vec(&document)
            .map_err(|source| InstallStateError::Serialize { source })?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|source| io_error("open", &temporary, source))?;
        file.write_all(&bytes)
            .map_err(|source| io_error("write", &temporary, source))?;
        file.sync_all()
            .map_err(|source| io_error("flush", &temporary, source))?;
        drop(file);
        fs::rename(&temporary, destination)
            .map_err(|source| io_error("rename", destination, source))?;
        Ok(())
    }

    /// 原子写入明确的“已安装但未激活”权威状态，不删除 deployment pointer。
    ///
    /// :param changed_at: 进入未激活状态的 UTC 时间。
    /// :return: 文件 flush 并原子替换完成时返回。
    /// :raises InstallStateError: 设置目录创建、序列化或写入失败时返回。
    pub fn mark_uninstalled(&self, changed_at: &str) -> Result<(), InstallStateError> {
        let destination = self.layout.deployment_file();
        let parent = destination.parent().ok_or(InstallStateError::PathEscape {
            field: "deployment",
        })?;
        fs::create_dir_all(parent).map_err(|source| io_error("create_dir_all", parent, source))?;
        let bytes = serde_json::to_vec(&UninstalledDocument {
            schema: DEPLOYMENT_SCHEMA,
            status: "uninstalled".to_owned(),
            changed_at: changed_at.to_owned(),
        })
        .map_err(|source| InstallStateError::Serialize { source })?;
        write_atomic(
            parent,
            destination,
            "deployment-uninstalled.json.tmp",
            &bytes,
        )
    }
}

pub(crate) fn validate_project_workspace(path: &Path) -> Result<PathBuf, InstallStateError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| InstallStateError::InvalidProjectWorkspace)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || !path.is_absolute() {
        return Err(InstallStateError::InvalidProjectWorkspace);
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| InstallStateError::InvalidProjectWorkspace)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(InstallStateError::InvalidProjectWorkspace);
        }
    }
    Ok(canonical)
}

fn write_atomic(
    parent: &Path,
    destination: &Path,
    temporary_name: &str,
    bytes: &[u8],
) -> Result<(), InstallStateError> {
    let temporary = parent.join(temporary_name);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)
        .map_err(|source| io_error("open", &temporary, source))?;
    file.write_all(bytes)
        .map_err(|source| io_error("write", &temporary, source))?;
    file.sync_all()
        .map_err(|source| io_error("flush", &temporary, source))?;
    drop(file);
    fs::rename(&temporary, destination).map_err(|source| io_error("rename", destination, source))
}

fn validate_deployment_directory(
    target: &'static str,
    path: &Path,
) -> Result<(), InstallStateError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(InstallStateError::DeploymentTargetMissing { target });
        }
        Err(source) => return Err(io_error("metadata", path, source)),
    };
    if !metadata.is_dir() {
        return Err(InstallStateError::DeploymentTargetNotDirectory { target });
    }
    Ok(())
}

fn validate_relative_dir(
    field: &'static str,
    actual: &str,
    expected: PathBuf,
) -> Result<(), InstallStateError> {
    let actual = Path::new(actual);
    let is_plain_relative = actual.is_relative()
        && actual
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if !is_plain_relative || actual != expected {
        return Err(InstallStateError::PathEscape { field });
    }
    Ok(())
}

fn io_error(operation: &'static str, path: &Path, source: std::io::Error) -> InstallStateError {
    InstallStateError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveDeployment, DataGeneration, InstallStateError, InstallStateStore, InstalledRuntime,
    };
    use crate::paths::{AppPaths, RuntimeLayout};
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_paths(name: &str) -> AppPaths {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间应晚于 Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dsh-install-state-{name}-{unique}"));
        AppPaths::from_roots(&root.join("roaming"), &root.join("local"))
    }

    fn deployment(version: &str, generation: &str) -> ActiveDeployment {
        ActiveDeployment::with_project_workspace(
            InstalledRuntime::with_node_version(version, "a".repeat(64), "24.15.0")
                .expect("测试版本应有效"),
            DataGeneration::new(generation).expect("测试 generation 应有效"),
            "2026-08-21T09:30:00Z".to_owned(),
            std::env::current_dir()
                .expect("测试 cwd")
                .canonicalize()
                .expect("测试 cwd canonical"),
        )
    }

    fn prepare_deployment_dirs(layout: &RuntimeLayout, deployment: &ActiveDeployment) {
        fs::create_dir_all(layout.runtime_dir(&deployment.runtime))
            .expect("应能创建 runtime 测试目录");
        fs::create_dir_all(layout.generation_dir(&deployment.data))
            .expect("应能创建 generation 测试目录");
    }

    #[test]
    fn explicit_uninstalled_state_survives_offline_restart_without_deleting_pointer() {
        let paths = test_paths("uninstalled");
        let store = InstallStateStore::new(RuntimeLayout::from_paths(&paths));

        store
            .mark_uninstalled("2026-08-22T00:00:00Z")
            .expect("应能原子写入未激活状态");

        assert!(paths.settings.join("deployment.json").is_file());
        assert!(matches!(store.load(), Err(InstallStateError::NotInstalled)));
    }

    #[test]
    fn uninstalled_pointer_rejects_unknown_schema_and_fields() {
        let paths = test_paths("uninstalled-strict");
        let store = InstallStateStore::new(RuntimeLayout::from_paths(&paths));
        fs::create_dir_all(&paths.settings).expect("settings");
        for json in [
            r#"{"schema":2,"status":"uninstalled","changed_at":"2026-08-22T00:00:00Z"}"#,
            r#"{"schema":1,"status":"uninstalled","changed_at":"2026-08-22T00:00:00Z","extra":true}"#,
        ] {
            fs::write(paths.settings.join("deployment.json"), json).expect("pointer");
            assert!(!matches!(
                store.load(),
                Err(InstallStateError::NotInstalled)
            ));
        }
    }

    #[test]
    fn installed_runtime_rejects_non_strict_semver() {
        for invalid in ["1", "1.2", "v1.2.3", "1.2.3/escape", " 1.2.3"] {
            assert!(matches!(
                InstalledRuntime::new(invalid, "a".repeat(64), "24.15.0"),
                Err(InstallStateError::InvalidVersion { .. })
            ));
        }
    }

    #[test]
    fn installed_runtime_rejects_non_canonical_manifest_digest_without_echoing_it() {
        for invalid in [
            "a".repeat(63),
            "a".repeat(65),
            "g".repeat(64),
            "A".repeat(64),
        ] {
            let error = InstalledRuntime::new("1.2.3", invalid.clone(), "24.15.0")
                .expect_err("非规范 SHA-256 摘要必须被拒绝");
            assert!(matches!(error, InstallStateError::InvalidManifestDigest));
            assert!(!error.to_string().contains(&invalid));
        }
    }

    #[test]
    fn layout_keeps_version_and_generation_inside_their_roots() {
        let paths = test_paths("layout");
        let layout = RuntimeLayout::from_paths(&paths);
        let active = deployment("1.2.3-rc.1", "generation-001");

        assert_eq!(
            layout.runtime_dir(&active.runtime),
            paths.runtimes.join("dsh").join("1.2.3-rc.1")
        );
        assert_eq!(
            layout.generation_dir(&active.data),
            paths.dsh_home.join("generations").join("generation-001")
        );
    }

    #[test]
    fn load_reports_missing_truncated_and_unknown_schema_separately() {
        let paths = test_paths("invalid-json");
        let store = InstallStateStore::new(RuntimeLayout::from_paths(&paths));

        assert!(matches!(store.load(), Err(InstallStateError::NotInstalled)));
        fs::create_dir_all(&paths.settings).expect("应能创建设置目录");
        fs::write(paths.settings.join("deployment.json"), b"{\"schema\":")
            .expect("应能写入截断 JSON");
        assert!(matches!(
            store.load(),
            Err(InstallStateError::InvalidJson { .. })
        ));

        fs::write(
            paths.settings.join("deployment.json"),
            br#"{"schema":2,"runtime":{"version":"1.2.3","relative_dir":"dsh/1.2.3","manifest_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"data":{"id":"generation-001","relative_dir":"generations/generation-001"},"activated_at":"2026-08-21T09:30:00Z"}"#,
        )
        .expect("应能写入未知 schema JSON");
        assert!(matches!(
            store.load(),
            Err(InstallStateError::UnknownSchema { schema: 2 })
        ));
    }

    #[test]
    fn load_rejects_runtime_or_generation_path_escape() {
        let paths = test_paths("escape");
        let store = InstallStateStore::new(RuntimeLayout::from_paths(&paths));
        fs::create_dir_all(&paths.settings).expect("应能创建设置目录");

        for (field, runtime_dir, data_dir) in [
            ("runtime", "../outside", "generations/generation-001"),
            ("data", "dsh/1.2.3", "../outside"),
        ] {
            let workspace = serde_json::to_string(
                &std::env::current_dir()
                    .expect("测试 cwd")
                    .canonicalize()
                    .expect("测试 cwd canonical")
                    .to_string_lossy(),
            )
            .expect("workspace json");
            let json = format!(
                r#"{{"schema":1,"runtime":{{"version":"1.2.3","relative_dir":"{runtime_dir}","manifest_digest":"{}","node_version":"24.15.0"}},"data":{{"id":"generation-001","relative_dir":"{data_dir}"}},"activated_at":"2026-08-21T09:30:00Z","project_workspace":{workspace}}}"#,
                "a".repeat(64)
            );
            fs::write(paths.settings.join("deployment.json"), json).expect("应能写入逃逸夹具");
            assert!(matches!(
                store.load(),
                Err(InstallStateError::PathEscape { field: actual }) if actual == field
            ));
        }
    }

    #[test]
    fn save_atomically_replaces_existing_deployment_pair() {
        let paths = test_paths("replace");
        let layout = RuntimeLayout::from_paths(&paths);
        let store = InstallStateStore::new(layout.clone());
        let first = deployment("1.2.3", "generation-001");
        let second = deployment("1.2.4", "generation-002");
        prepare_deployment_dirs(&layout, &first);
        prepare_deployment_dirs(&layout, &second);

        store.save(&first).expect("首次写入应成功");
        store.save(&second).expect("目标已存在时也应原子替换");

        assert_eq!(store.load().expect("应能读取替换后的状态"), second);
        assert!(layout.runtime_dir(&first.runtime).is_dir());
        assert!(layout.generation_dir(&first.data).is_dir());
        assert!(layout.runtime_dir(&second.runtime).is_dir());
        assert!(layout.generation_dir(&second.data).is_dir());
        assert!(!paths.settings.join("deployment.json.tmp").exists());
    }

    #[test]
    fn save_rejects_missing_runtime_or_generation_without_creating_pointer() {
        for (missing, create_runtime, create_generation) in
            [("runtime", false, true), ("generation", true, false)]
        {
            let paths = test_paths(missing);
            let layout = RuntimeLayout::from_paths(&paths);
            let store = InstallStateStore::new(layout.clone());
            let active = deployment("1.2.3", "generation-001");
            if create_runtime {
                fs::create_dir_all(layout.runtime_dir(&active.runtime))
                    .expect("应能创建 runtime 测试目录");
            }
            if create_generation {
                fs::create_dir_all(layout.generation_dir(&active.data))
                    .expect("应能创建 generation 测试目录");
            }

            assert!(matches!(
                store.save(&active),
                Err(InstallStateError::DeploymentTargetMissing { target }) if target == missing
            ));
            assert!(!paths.settings.join("deployment.json").exists());
        }
    }

    #[test]
    fn save_rejects_file_target_and_preserves_existing_deployment() {
        let paths = test_paths("target-file");
        let layout = RuntimeLayout::from_paths(&paths);
        let store = InstallStateStore::new(layout.clone());
        let first = deployment("1.2.3", "generation-001");
        prepare_deployment_dirs(&layout, &first);
        store.save(&first).expect("初始 deployment 应写入成功");

        let second = deployment("1.2.4", "generation-002");
        let runtime_file = layout.runtime_dir(&second.runtime);
        fs::create_dir_all(runtime_file.parent().expect("runtime 应有父目录"))
            .expect("应能创建 runtime 根目录");
        fs::write(&runtime_file, b"not-a-directory").expect("应能创建同名文件夹具");
        fs::create_dir_all(layout.generation_dir(&second.data))
            .expect("应能创建 generation 测试目录");

        assert!(matches!(
            store.save(&second),
            Err(InstallStateError::DeploymentTargetNotDirectory { target: "runtime" })
        ));
        assert_eq!(store.load().expect("旧 deployment 不得被修改"), first);
    }

    #[test]
    fn deployment_json_does_not_store_absolute_download_url() {
        let paths = test_paths("serialized-fields");
        let layout = RuntimeLayout::from_paths(&paths);
        let store = InstallStateStore::new(layout.clone());
        let active = deployment("1.2.3", "generation-001");
        prepare_deployment_dirs(&layout, &active);

        store.save(&active).expect("写入应成功");
        let json = fs::read_to_string(paths.settings.join("deployment.json"))
            .expect("应能读取 deployment JSON");

        assert!(!json.contains("http://"));
        assert!(!json.contains("https://"));
        assert!(!json.contains(&paths.runtimes.display().to_string()));
        assert!(!json.contains(&paths.dsh_home.display().to_string()));
        assert!(json.contains(r#""relative_dir":"dsh/1.2.3""#));
        assert!(json.contains(r#""relative_dir":"generations/generation-001""#));
    }

    #[test]
    fn generation_id_rejects_path_components() {
        for invalid in [
            "",
            ".",
            "..",
            ".state",
            ".STATE",
            ".state.",
            ".dsh-internal-x",
            ".DSH-INTERNAL-x",
            "CON",
            "nul.json",
            "a/b",
            r"a\b",
            "C:escape",
        ] {
            assert!(matches!(
                DataGeneration::new(invalid),
                Err(InstallStateError::InvalidGeneration { .. })
            ));
        }
        assert!(Path::new("generation-001").is_relative());
    }
}
