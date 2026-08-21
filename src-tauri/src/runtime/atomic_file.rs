#[cfg(not(windows))]
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;

/// 将同卷临时文件原子替换为目标文件，并尽力确保内容与目录项落盘。
///
/// 调用方应先在目标文件所在目录创建临时源文件。同卷约束保证替换不会退化为
/// 复制；Windows 的 `WRITE_THROUGH` 与非 Windows 的父目录同步用于缩小断电后
/// 丢失已提交指针的窗口。
///
/// :param source: 已完整写入、即将被移动的临时文件。
/// :param target: 可已存在的最终目标文件。
/// :return: 替换与持久化步骤均成功时返回 `Ok(())`。
/// :raises io::Error: 同步、路径转换或原子替换失败时返回底层 I/O 错误。
pub(crate) fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    // 先同步文件内容，避免目录项已切换但新内容仍只停留在系统缓存中。
    OpenOptions::new().write(true).open(source)?.sync_all()?;
    replace_file_platform(source, target)
}

#[cfg(windows)]
fn replace_file_platform(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target_wide: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();

    // 两个标志共同保证已有目标可被原子替换，并要求系统等待移动操作落盘。
    unsafe {
        MoveFileExW(
            PCWSTR(source_wide.as_ptr()),
            PCWSTR(target_wide.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(io::Error::other)
}

#[cfg(not(windows))]
fn replace_file_platform(source: &Path, target: &Path) -> io::Result<()> {
    std::fs::rename(source, target)?;
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "目标文件缺少父目录"))?;
    // POSIX rename 成功后再同步父目录，使新的目录项跨崩溃保持可见。
    File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::replace_file;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn replace_file_replaces_existing_target_and_moves_source() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间应晚于 Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "dsh-desktop-atomic-file-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("应创建隔离测试目录");
        let source = directory.join("deployment.json.tmp");
        let target = directory.join("deployment.json");
        fs::write(&source, br#"{"runtime":"0.2.0","generation":"candidate"}"#)
            .expect("应写入临时源文件");
        fs::write(&target, br#"{"runtime":"0.1.0","generation":"active"}"#)
            .expect("应写入已有目标文件");

        replace_file(&source, &target).expect("原子替换应成功");

        assert_eq!(
            fs::read(&target).expect("应读取替换后的目标"),
            br#"{"runtime":"0.2.0","generation":"candidate"}"#
        );
        assert!(!source.exists(), "成功替换后临时源应已被移动");
    }
}
