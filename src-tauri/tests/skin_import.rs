use dsh_desktop_lib::skin::{SkinErrorKind, SkinFormat, SkinImporter};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const PNG_2X1: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x7b, 0x40, 0xe8,
    0xdd, 0x00, 0x00, 0x00, 0x0f, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x0c, 0x58, 0xf5, 0x81,
    0x81, 0x81, 0x01, 0x00, 0x09, 0x00, 0x01, 0xec, 0x7c, 0x09, 0x92, 0xbe, 0x00, 0x00, 0x00, 0x00,
    0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

const WEBP_2X1: &[u8] = &[
    0x52, 0x49, 0x46, 0x46, 0x3c, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x20,
    0x30, 0x00, 0x00, 0x00, 0xf0, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x02, 0x00, 0x01, 0x00, 0x01, 0x40,
    0x26, 0x25, 0xa0, 0x02, 0x74, 0xba, 0x01, 0xf8, 0x00, 0x04, 0x83, 0x00, 0x00, 0xfe, 0xea, 0x2a,
    0xff, 0xfc, 0xf4, 0xcd, 0x79, 0x83, 0xfc, 0x9c, 0xff, 0xe8, 0xc3, 0xf8, 0xd2, 0x3c, 0x69, 0x1f,
    0x0e, 0x60, 0x00, 0x00,
];

const JPEG_2X1_HEX: &str = concat!(
    "ffd8ffe000104a46494600010100000100010000ffdb004300080606070605080707070909080a0c140d0c0b0b0c1912130f141d1a1f1e1d1a1c1c20242e2720222c231c1c2837292c30313434341f27393d38323c2e333432",
    "ffdb0043010909090c0b0c180d0d1832211c213232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232",
    "ffc00011080001000203012200021101031101ffc4001f0000010501010101010100000000000000000102030405060708090a0b",
    "ffc400b5100002010303020403050504040000017d01020300041105122131410613516107227114328191a1082342b1c11552d1f02433627282090a161718191a25262728292a3435363738393a434445464748494a535455565758595a636465666768696a737475767778797a838485868788898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae1e2e3e4e5e6e7e8e9eaf1f2f3f4f5f6f7f8f9fa",
    "ffc4001f0100030101010101010101010000000000000102030405060708090a0b",
    "ffc400b51100020102040403040705040400010277000102031104052131061241510761711322328108144291a1b1c109233352f0156272d10a162434e125f11718191a262728292a35363738393a434445464748494a535455565758595a636465666768696a737475767778797a82838485868788898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae2e3e4e5e6e7e8e9eaf2f3f4f5f6f7f8f9fa",
    "ffda000c03010002110311003f00d7a28a2beb0f923fffd9"
);

const MAX_SKIN_BYTES: usize = 20 * 1024 * 1024;
static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn fixture_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间应晚于 Unix epoch")
        .as_nanos();
    let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "dsh-desktop-skin-import-{}-{name}-{nonce}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("skins")).expect("应创建隔离皮肤目录");
    root
}

fn write_source(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let source = root.join(name);
    fs::write(&source, bytes).expect("应写入隔离图片夹具");
    source
}

fn importer(root: &Path) -> SkinImporter {
    SkinImporter::new(root.join("skins"))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn assert_kind<T>(result: Result<T, dsh_desktop_lib::skin::SkinError>, expected: SkinErrorKind) {
    let error = result.err().expect("此夹具必须被拒绝");
    assert_eq!(error.kind(), expected);
    assert!(
        !error.to_string().contains("skin-import"),
        "错误不得泄露路径"
    );
}

#[tokio::test]
async fn imports_valid_png_by_content_not_extension_or_unicode_name() {
    // 若实现退回扩展名判断，`.txt` 会使这个真实 PNG 无法导入。
    let root = fixture_root("content-detection");
    let source = write_source(&root, "天空背景.txt", PNG_2X1);

    let image = importer(&root).import(source).await.expect("应导入 PNG");

    assert_eq!(image.format, SkinFormat::Png);
    assert_eq!((image.width, image.height), (2, 1));
    assert_eq!(image.byte_size, PNG_2X1.len() as u64);
    assert_eq!(image.digest, digest(PNG_2X1));
    assert_eq!(
        image.path,
        root.join("skins").join(format!("{}.png", image.digest))
    );
    assert_eq!(fs::read(image.path).expect("应读取托管副本"), PNG_2X1);
}

#[tokio::test]
async fn imports_valid_webp_and_is_idempotent() {
    // 若 create_new 的已存在分支不复核内容，第二次导入可能错误失败或接受污染文件。
    let root = fixture_root("idempotent-webp");
    let first_source = write_source(&root, "first.bin", WEBP_2X1);
    let second_source = write_source(&root, "second.webp", WEBP_2X1);

    let first = importer(&root)
        .import(first_source)
        .await
        .expect("首次导入应成功");
    let second = importer(&root)
        .import(second_source)
        .await
        .expect("重复导入应幂等");

    assert_eq!(first, second);
    assert_eq!(first.format, SkinFormat::Webp);
    assert_eq!(fs::read(first.path).expect("应读取托管副本"), WEBP_2X1);
}

#[tokio::test]
async fn imports_valid_jpeg_with_canonical_jpg_extension() {
    // 若 JPEG 分支遗漏或扩展名不规范，允许格式清单与托管路径会不一致。
    let root = fixture_root("jpeg");
    let bytes = decode_hex(JPEG_2X1_HEX);
    let source = write_source(&root, "photo.data", &bytes);

    let image = importer(&root).import(source).await.expect("应导入 JPEG");

    assert_eq!(image.format, SkinFormat::Jpeg);
    assert_eq!((image.width, image.height), (2, 1));
    assert!(image.path.ends_with(format!("{}.jpg", image.digest)));
    assert_eq!(fs::read(image.path).expect("应读取托管副本"), bytes);
}

#[tokio::test]
async fn rejects_file_larger_than_the_bounded_read_limit() {
    // 若读取没有在 20 MiB + 1 停止，此测试会错误接受超限输入。
    let root = fixture_root("too-large");
    let source = write_source(&root, "large.png", &vec![0_u8; MAX_SKIN_BYTES + 1]);

    assert_kind(
        importer(&root).import(source).await,
        SkinErrorKind::TooLarge,
    );
}

#[tokio::test]
async fn rejects_over_edge_and_total_pixel_limits_before_decode() {
    // 两个夹具只修改 PNG IHDR；正确实现应先拒绝尺寸，不进入完整解码错误分支。
    let root = fixture_root("dimensions");
    let over_edge = write_source(&root, "over-edge.png", &png_with_dimensions(7681, 1));
    let over_pixels = write_source(&root, "over-pixels.png", &png_with_dimensions(7680, 4321));

    assert_kind(
        importer(&root).import(over_edge).await,
        SkinErrorKind::Dimensions,
    );
    assert_kind(
        importer(&root).import(over_pixels).await,
        SkinErrorKind::Dimensions,
    );
}

#[tokio::test]
async fn rejects_corruption_and_unsupported_gif_with_distinct_kinds() {
    // 格式受支持但数据截断属于 Decode；GIF 则必须稳定归类为 UnsupportedFormat。
    let root = fixture_root("format-errors");
    let corrupt_png = write_source(&root, "corrupt.png", &PNG_2X1[..40]);
    let gif = write_source(&root, "image.gif", b"GIF89a\x01\0\x01\0\0\0\0\0");

    assert_kind(
        importer(&root).import(corrupt_png).await,
        SkinErrorKind::Decode,
    );
    assert_kind(
        importer(&root).import(gif).await,
        SkinErrorKind::UnsupportedFormat,
    );
}

#[tokio::test]
async fn preexisting_digest_target_with_different_content_is_never_overwritten() {
    // 若已存在分支只按文件名接受，攻击者预置的不同内容会冒充摘要对应图片。
    let root = fixture_root("collision");
    let source = write_source(&root, "source.png", PNG_2X1);
    let target = root.join("skins").join(format!("{}.png", digest(PNG_2X1)));
    fs::write(&target, b"different").expect("应预置冲突目标");

    assert_kind(
        importer(&root).import(source).await,
        SkinErrorKind::FileSystem,
    );
    assert_eq!(fs::read(target).expect("冲突目标应保留"), b"different");
}

#[cfg(windows)]
#[tokio::test]
async fn rejects_multilink_managed_destination() {
    // 多硬链接会让托管目录外的路径获得修改同一文件的能力，因此不能作为不可变命中。
    let root = fixture_root("hardlink-target");
    let source = write_source(&root, "source.png", PNG_2X1);
    let target = root.join("skins").join(format!("{}.png", digest(PNG_2X1)));
    fs::write(&target, PNG_2X1).expect("应预置目标");
    fs::hard_link(&target, root.join("outside-link.png")).expect("应创建目标硬链接");

    assert_kind(
        importer(&root).import(source).await,
        SkinErrorKind::FileSystem,
    );
}

#[cfg(windows)]
#[tokio::test]
async fn rejects_reparse_managed_destination_without_touching_external_target() {
    use std::os::windows::fs::{MetadataExt, symlink_file};
    use std::process::Command;

    // 若已存在目标被按普通文件跟随，导入器会错误接受托管目录外的同内容文件，
    // 也会让未来的读取获得越过 skins 根目录的能力。
    let root = fixture_root("reparse-target");
    let source = write_source(&root, "source.png", PNG_2X1);
    let external_root = root.join("external-target");
    fs::create_dir(&external_root).expect("应创建托管目录外的目标目录");
    let external = write_source(&external_root, "external.png", PNG_2X1);
    let target = root.join("skins").join(format!("{}.png", digest(PNG_2X1)));
    if symlink_file(&external, &target).is_err() {
        // 未开启开发者模式时，普通用户仍可创建目录 junction；它同样是必须失败关闭的
        // Windows reparse point，并避免让安全测试静默跳过。
        let status = Command::new("pwsh.exe")
            .args([
                "-NoProfile",
                "-Command",
                "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:DSH_TEST_LINK -Target $env:DSH_TEST_TARGET | Out-Null",
            ])
            .env("DSH_TEST_LINK", &target)
            .env("DSH_TEST_TARGET", &external_root)
            .status()
            .expect("应启动 PowerShell 7 创建 junction 夹具");
        assert!(status.success(), "应创建 junction reparse 夹具");
    }
    let target_metadata = fs::symlink_metadata(&target).expect("应读取 reparse 夹具属性");
    assert_ne!(
        target_metadata.file_attributes() & 0x400,
        0,
        "目标夹具必须确实带有 FILE_ATTRIBUTE_REPARSE_POINT"
    );
    let before = fs::read(&external).expect("应读取外部目标夹具");

    assert_kind(
        importer(&root).import(source).await,
        SkinErrorKind::FileSystem,
    );
    assert_eq!(
        fs::read(&external).expect("应复核外部目标"),
        before,
        "拒绝 reparse 目标时不得修改托管目录外的文件"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn rejects_source_reparse_point_when_windows_can_create_fixture() {
    use std::os::windows::fs::symlink_file;

    // 打开源文件若跟随 reparse point，就可能越过用户实际选择的文件身份。
    let root = fixture_root("source-reparse");
    let real = write_source(&root, "real.png", PNG_2X1);
    let source = root.join("selected.png");
    if symlink_file(&real, &source).is_err() {
        // 非开发者模式的 Windows 可能禁止创建符号链接；其他安全夹具仍覆盖硬链接。
        return;
    }

    assert_kind(
        importer(&root).import(source).await,
        SkinErrorKind::FileSystem,
    );
}

fn png_with_dimensions(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = PNG_2X1.to_vec();
    bytes[16..20].copy_from_slice(&width.to_be_bytes());
    bytes[20..24].copy_from_slice(&height.to_be_bytes());
    let crc = crc32(&bytes[12..29]);
    bytes[29..33].copy_from_slice(&crc.to_be_bytes());
    bytes
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn decode_hex(value: &str) -> Vec<u8> {
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    assert!(remainder.is_empty(), "十六进制夹具长度必须为偶数");
    pairs
        .iter()
        .map(|pair| {
            let text = std::str::from_utf8(pair).expect("十六进制夹具应为 ASCII");
            u8::from_str_radix(text, 16).expect("十六进制夹具应有效")
        })
        .collect()
}
