use std::fs;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathError {
    #[error("无法解析系统目录: {0}")]
    Resolve(String),
    #[error("无法创建目录 {path}: {source}")]
    Create {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    pub dsh_home: PathBuf,
    pub settings: PathBuf,
    pub logs: PathBuf,
    pub runtimes: PathBuf,
    pub skins: PathBuf,
    pub updates: PathBuf,
    pub webview_data: PathBuf,
}

/// 已安装运行时、数据 generation 与激活指针的固定目录布局。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeLayout {
    runtime_root: PathBuf,
    generation_root: PathBuf,
    deployment_file: PathBuf,
}

impl RuntimeLayout {
    /// 从应用目录创建版本化运行时布局。
    ///
    /// :param paths: 已由系统根目录推导出的应用路径。
    /// :return: 运行时、generation 与激活指针的路径值对象。
    /// :raises: 此函数仅计算路径，不访问文件系统。
    pub fn from_paths(paths: &AppPaths) -> Self {
        Self {
            runtime_root: paths.runtimes.join("dsh"),
            generation_root: paths.dsh_home.join("generations"),
            deployment_file: paths.settings.join("deployment.json"),
        }
    }

    /// 返回某个已验证版本对应的不可变运行时目录。
    ///
    /// :param runtime: 已通过严格 semver 校验的运行时标识。
    /// :return: `runtimes/dsh/<version>` 下的绝对目录。
    /// :raises: 版本已在构造时校验，此函数不产生错误。
    pub fn runtime_dir(
        &self,
        runtime: &crate::runtime::install_state::InstalledRuntime,
    ) -> PathBuf {
        self.runtime_root.join(runtime.version.to_string())
    }

    /// 返回某个已验证 generation 对应的数据目录。
    ///
    /// :param data: 不包含路径分隔符的 generation 标识。
    /// :return: `dsh-home/generations/<id>` 下的绝对目录。
    /// :raises: generation 已在构造时校验，此函数不产生错误。
    pub fn generation_dir(&self, data: &crate::runtime::install_state::DataGeneration) -> PathBuf {
        self.generation_root.join(&data.id)
    }

    /// 返回所有隔离数据 generation 的固定根目录。
    ///
    /// :return: `dsh-home/generations` 路径。
    /// :raises: 此只读访问不产生错误。
    pub fn generation_root(&self) -> &Path {
        &self.generation_root
    }

    /// 返回所有不可变 DSH runtime 的固定根目录。
    ///
    /// :return: `runtimes/dsh` 路径。
    /// :raises: 此只读访问不产生错误。
    pub fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }

    pub(crate) fn deployment_file(&self) -> &Path {
        &self.deployment_file
    }

    /// 返回 deployment 与 activation journal 共用的漫游设置目录。
    pub(crate) fn settings_dir(&self) -> Result<&Path, PathError> {
        self.deployment_file
            .parent()
            .ok_or_else(|| PathError::Resolve("deployment 文件缺少设置目录".to_owned()))
    }
}

impl AppPaths {
    /// 根据 Windows 漫游目录与本地目录计算应用的固定目录布局。
    ///
    /// 配置、日志和 DSH 主目录属于用户数据，放入漫游目录；可重新下载或生成的
    /// 运行时、皮肤、更新和 WebView 数据放入本地目录，避免缓存污染用户数据。
    ///
    /// :param roaming: Windows 漫游应用数据目录。
    /// :param local: Windows 本地应用数据目录。
    /// :return: 尚未在文件系统中创建的路径值对象。
    /// :raises: 此函数仅计算路径，不产生错误。
    pub fn from_roots(roaming: &Path, local: &Path) -> Self {
        let roaming_root = roaming.join("DSH Desktop");
        let local_root = local.join("DSH Desktop");
        Self {
            dsh_home: roaming_root.join("dsh-home"),
            settings: roaming_root.join("settings"),
            logs: roaming_root.join("logs"),
            runtimes: local_root.join("runtimes"),
            skins: local_root.join("skins"),
            updates: local_root.join("updates"),
            webview_data: local_root.join("webview-data"),
        }
    }

    /// 从 Tauri 提供的系统路径解析应用目录布局。
    ///
    /// :param app: 当前 Tauri 应用句柄。
    /// :return: 解析成功的路径值对象。
    /// :raises PathError: 系统目录不可用时返回 `PathError::Resolve`。
    pub fn resolve(app: &AppHandle) -> Result<Self, PathError> {
        let roaming = app
            .path()
            .config_dir()
            .map_err(|error| PathError::Resolve(error.to_string()))?;
        let local = app
            .path()
            .local_data_dir()
            .map_err(|error| PathError::Resolve(error.to_string()))?;
        Ok(Self::from_roots(&roaming, &local))
    }

    /// 创建运行 DSH Desktop 所需的全部预定义目录。
    ///
    /// :return: 全部目录存在时返回 `Ok(())`。
    /// :raises PathError: 任一目录无法创建时返回路径及底层 I/O 错误。
    pub fn ensure_exists(&self) -> Result<(), PathError> {
        for path in [
            &self.dsh_home,
            &self.settings,
            &self.logs,
            &self.runtimes,
            &self.skins,
            &self.updates,
            &self.webview_data,
        ] {
            fs::create_dir_all(path).map_err(|source| PathError::Create {
                path: path.clone(),
                source,
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::AppPaths;
    use std::path::Path;

    #[test]
    fn fixed_roots_keep_user_data_separate_from_runtime_cache() {
        let paths = AppPaths::from_roots(
            Path::new(r"C:\Users\demo\AppData\Roaming"),
            Path::new(r"C:\Users\demo\AppData\Local"),
        );

        assert!(paths.dsh_home.ends_with(r"DSH Desktop\dsh-home"));
        assert!(paths.settings.ends_with(r"DSH Desktop\settings"));
        assert!(paths.runtimes.ends_with(r"DSH Desktop\runtimes"));
        assert!(paths.webview_data.ends_with(r"DSH Desktop\webview-data"));
        assert!(!paths.dsh_home.starts_with(&paths.runtimes));
    }

    #[test]
    fn non_ascii_roots_are_preserved_as_path_values() {
        let unicode_root = std::env::temp_dir().join("鲸鱼 用户");
        let paths = AppPaths::from_roots(&unicode_root, &unicode_root);

        assert_eq!(
            paths.dsh_home,
            unicode_root.join("DSH Desktop").join("dsh-home")
        );
        assert_eq!(
            paths.webview_data,
            unicode_root.join("DSH Desktop").join("webview-data")
        );
    }
}
