use dsh_desktop_lib::skin::{
    MaskTone, SkinDraft, SkinErrorKind, SkinFit, SkinPosition, SkinSettings, SkinStore,
};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn fixture_store(name: &str) -> (SkinStore, PathBuf) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间应晚于 Unix epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "dsh-desktop-skin-store-{}-{name}-{nonce}",
        std::process::id()
    ));
    let settings = root.join("settings");
    let skins = root.join("skins");
    fs::create_dir_all(&settings).expect("应创建隔离设置目录");
    fs::create_dir_all(&skins).expect("应创建隔离皮肤目录");
    (SkinStore::new(settings, skins), root)
}

fn valid_draft() -> SkinDraft {
    SkinDraft {
        immersive: false,
        image_digest: None,
        fit: SkinFit::Cover,
        position: SkinPosition::Center,
        blur_px: 12,
        glass_blur_px: 0,
        mask_tone: MaskTone::Light,
        mask_opacity_percent: 24,
        panel_opacity_percent: 86,
        conversation_surface_opacity_percent: 85,
    }
}

fn immersive_draft() -> SkinDraft {
    SkinDraft {
        immersive: true,
        image_digest: Some(DIGEST.to_owned()),
        ..valid_draft()
    }
}

fn persisted_json(revision: u64, settings: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema": 1,
        "revision": revision,
        "settings": settings,
    }))
    .expect("应编码设置夹具")
}

fn valid_settings_json() -> serde_json::Value {
    serde_json::json!({
        "immersive": false,
        "image_digest": null,
        "fit": "cover",
        "position": "center",
        "blur_px": 12,
        "mask_tone": "light",
        "mask_opacity_percent": 24,
        "panel_opacity_percent": 86,
    })
}

fn valid_settings_v2_json() -> serde_json::Value {
    let mut settings = valid_settings_json();
    settings
        .as_object_mut()
        .expect("设置夹具应为对象")
        .insert("glass_blur_px".to_owned(), serde_json::json!(0));
    settings
}

#[test]
fn defaults_are_non_immersive_and_bounded() {
    let settings = SkinSettings::default();
    assert!(!settings.immersive);
    assert_eq!(settings.fit, SkinFit::Cover);
    assert_eq!(settings.position, SkinPosition::Center);
    assert_eq!(settings.blur_px, 0);
    assert_eq!(settings.mask_opacity_percent, 22);
    assert_eq!(settings.panel_opacity_percent, 88);
}

#[test]
fn glass_blur_defaults_to_zero_and_rejects_values_above_32px() {
    let settings = SkinSettings::default();
    assert_eq!(settings.glass_blur_px, 0);

    let (store, _) = fixture_store("glass-blur-range");
    let accepted = store
        .save(
            0,
            SkinDraft {
                glass_blur_px: 32,
                ..valid_draft()
            },
        )
        .expect("32px 应处于闭区间内");
    assert_eq!(accepted.settings.glass_blur_px, 32);

    let error = store
        .save(
            accepted.revision,
            SkinDraft {
                glass_blur_px: 33,
                ..valid_draft()
            },
        )
        .expect_err("33px 必须被拒绝");
    assert_eq!(error.kind(), SkinErrorKind::InvalidSettings);
}

#[test]
fn schema_one_loads_without_writing_and_next_save_upgrades_to_current_schema() {
    let (store, root) = fixture_store("schema-one-migration");
    let path = root.join("settings").join("skin.json");
    fs::write(&path, persisted_json(7, valid_settings_json())).expect("应写入 schema 1 夹具");

    let loaded = store.load().expect("schema 1 应迁移到内存");
    assert_eq!(loaded.revision, 7);
    assert_eq!(loaded.settings.glass_blur_px, 0);
    let disk_before_save: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("应读取旧设置")).expect("应解析旧设置");
    assert_eq!(disk_before_save["schema"], 1);

    let settings = loaded.settings;
    store
        .save(
            loaded.revision,
            SkinDraft {
                immersive: settings.immersive,
                image_digest: settings.image_digest,
                fit: settings.fit,
                position: settings.position,
                blur_px: settings.blur_px,
                glass_blur_px: settings.glass_blur_px,
                mask_tone: settings.mask_tone,
                mask_opacity_percent: settings.mask_opacity_percent,
                panel_opacity_percent: settings.panel_opacity_percent,
                conversation_surface_opacity_percent: settings.conversation_surface_opacity_percent,
            },
        )
        .expect("显式保存应升级设置");

    let disk_after_save: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("应读取新设置")).expect("应解析新设置");
    assert_eq!(disk_after_save["schema"], 3);
    assert_eq!(disk_after_save["settings"]["glass_blur_px"], 0);
    assert_eq!(
        disk_after_save["settings"]["conversation_surface_opacity_percent"],
        85
    );
}

#[test]
fn schema_two_loads_with_default_conversation_surface_opacity() {
    let (store, root) = fixture_store("schema-two-conversation-surface-default");
    fs::write(
        root.join("settings").join("skin.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": 2,
            "revision": 9,
            "settings": valid_settings_v2_json(),
        }))
        .expect("应编码 schema 2 夹具"),
    )
    .expect("应写入 schema 2 夹具");

    let loaded = store.load().expect("schema 2 应迁移到当前内存模型");
    let settings = serde_json::to_value(loaded.settings).expect("设置应可序列化");

    assert_eq!(settings["conversation_surface_opacity_percent"], 85);
}

#[test]
fn schema_two_requires_a_bounded_integer_glass_blur_without_unknown_fields() {
    let cases = [
        ("missing", {
            let mut settings = valid_settings_v2_json();
            settings
                .as_object_mut()
                .expect("设置夹具应为对象")
                .remove("glass_blur_px");
            settings
        }),
        ("unknown", {
            let mut settings = valid_settings_v2_json();
            settings
                .as_object_mut()
                .expect("设置夹具应为对象")
                .insert("unexpected".to_owned(), serde_json::json!(true));
            settings
        }),
        ("negative", {
            let mut settings = valid_settings_v2_json();
            settings["glass_blur_px"] = serde_json::json!(-1);
            settings
        }),
        ("fraction", {
            let mut settings = valid_settings_v2_json();
            settings["glass_blur_px"] = serde_json::json!(1.5);
            settings
        }),
        ("above-maximum", {
            let mut settings = valid_settings_v2_json();
            settings["glass_blur_px"] = serde_json::json!(33);
            settings
        }),
    ];

    for (name, settings) in cases {
        let (store, root) = fixture_store(name);
        fs::write(
            root.join("settings").join("skin.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema": 2,
                "revision": 4,
                "settings": settings,
            }))
            .expect("应编码 schema 2 夹具"),
        )
        .expect("应写入 schema 2 夹具");
        assert_eq!(
            store.load().expect_err("无效 schema 2 必须失败关闭").kind(),
            SkinErrorKind::CorruptSettings,
            "case: {name}",
        );
    }
}

#[test]
fn stale_revision_cannot_overwrite_newer_skin_settings() {
    let (store, _) = fixture_store("stale-revision");
    let first = store.save(0, valid_draft()).expect("首次保存应成功");

    let error = store
        .save(0, valid_draft())
        .expect_err("旧 revision 不得覆盖新设置");

    assert_eq!(error.kind(), SkinErrorKind::RevisionConflict);
    assert_eq!(
        store.load().expect("应读取当前设置").revision,
        first.revision
    );
}

#[test]
fn reset_changes_only_the_pointer_and_keeps_imported_files() {
    let (store, root) = fixture_store("reset-keeps-history");
    let image = root.join("skins").join(format!("{DIGEST}.png"));
    fs::write(&image, b"registered image fixture").expect("应登记测试图片");
    store.save(0, immersive_draft()).expect("应保存沉浸设置");

    let reset = store.reset(1).expect("应恢复默认设置");

    assert_eq!(reset.revision, 2);
    assert!(!reset.settings.immersive);
    assert!(reset.settings.image_digest.is_none());
    assert_eq!(reset.settings.glass_blur_px, 0);
    assert!(image.exists(), "恢复默认不得删除历史图片");
}

#[test]
fn visual_bounds_and_noncanonical_digests_are_rejected() {
    let (store, root) = fixture_store("validation");
    fs::write(root.join("skins").join(format!("{DIGEST}.png")), b"image").expect("应登记测试图片");

    let invalid = [
        SkinDraft {
            blur_px: 33,
            ..valid_draft()
        },
        SkinDraft {
            mask_opacity_percent: 81,
            ..valid_draft()
        },
        SkinDraft {
            panel_opacity_percent: 101,
            ..valid_draft()
        },
        SkinDraft {
            conversation_surface_opacity_percent: 101,
            ..valid_draft()
        },
        SkinDraft {
            image_digest: Some(DIGEST.to_uppercase()),
            ..valid_draft()
        },
    ];

    for draft in invalid {
        assert_eq!(
            store.save(0, draft).expect_err("越界设置必须被拒绝").kind(),
            SkinErrorKind::InvalidSettings
        );
    }
    assert_eq!(store.load().expect("拒绝后应保留默认值").revision, 0);
}

#[test]
fn image_opacity_accepts_the_full_percentage_range() {
    let (store, _) = fixture_store("image-opacity-range");

    let hidden = store
        .save(
            0,
            SkinDraft {
                panel_opacity_percent: 0,
                ..valid_draft()
            },
        )
        .expect("图片不透明度 0% 应有效");
    let visible = store
        .save(
            hidden.revision,
            SkinDraft {
                panel_opacity_percent: 100,
                ..valid_draft()
            },
        )
        .expect("图片不透明度 100% 应有效");

    assert_eq!(hidden.settings.panel_opacity_percent, 0);
    assert_eq!(visible.settings.panel_opacity_percent, 100);
}

#[test]
fn immersive_mode_requires_a_registered_digest_file() {
    let (store, _) = fixture_store("registered-image");

    let error = store
        .save(0, immersive_draft())
        .expect_err("未登记图片不得启用沉浸模式");

    assert_eq!(error.kind(), SkinErrorKind::ImageNotRegistered);
}

#[test]
fn persisted_json_is_strict_and_round_trips_revision() {
    let (store, root) = fixture_store("strict-json");
    let saved = store.save(0, valid_draft()).expect("应保存设置");
    let reopened = SkinStore::new(root.join("settings"), root.join("skins"));

    assert_eq!(reopened.load().expect("应重新读取设置"), saved);

    let path = root.join("settings").join("skin.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("应读取设置文件")).expect("应解析 JSON");
    value
        .as_object_mut()
        .expect("根对象")
        .insert("unexpected".to_owned(), serde_json::Value::Bool(true));
    fs::write(&path, serde_json::to_vec(&value).expect("应编码 JSON")).expect("应写入损坏设置");

    assert_eq!(
        reopened.load().expect_err("未知字段必须失败关闭").kind(),
        SkinErrorKind::CorruptSettings
    );
}

#[test]
fn settings_file_above_fixed_limit_is_corrupt_without_unbounded_read() {
    let (store, root) = fixture_store("oversized-json");
    let path = root.join("settings").join("skin.json");
    fs::write(&path, vec![b' '; 64 * 1024 + 1]).expect("应写入超限夹具");

    assert_eq!(
        store.load().expect_err("超限设置必须失败关闭").kind(),
        SkinErrorKind::CorruptSettings
    );
}

#[test]
fn invalid_values_loaded_from_disk_are_classified_as_corrupt_settings() {
    let cases = [
        serde_json::json!({ "blur_px": 33 }),
        serde_json::json!({ "image_digest": DIGEST.to_uppercase() }),
        serde_json::json!({ "immersive": true, "image_digest": DIGEST }),
    ];

    for (index, replacement) in cases.into_iter().enumerate() {
        let (store, root) = fixture_store(&format!("corrupt-value-{index}"));
        let mut settings = valid_settings_json();
        for (key, value) in replacement.as_object().expect("替换值应为对象") {
            settings
                .as_object_mut()
                .expect("设置应为对象")
                .insert(key.clone(), value.clone());
        }
        fs::write(
            root.join("settings").join("skin.json"),
            persisted_json(7, settings),
        )
        .expect("应写入损坏设置夹具");

        assert_eq!(
            store.load().expect_err("磁盘损坏必须稳定归类").kind(),
            SkinErrorKind::CorruptSettings
        );
    }
}

#[test]
fn maximum_revision_cannot_be_saved_without_advancing() {
    let (store, root) = fixture_store("revision-overflow");
    fs::write(
        root.join("settings").join("skin.json"),
        persisted_json(u64::MAX, valid_settings_json()),
    )
    .expect("应写入最大 revision 夹具");

    assert_eq!(
        store
            .save(u64::MAX, valid_draft())
            .expect_err("revision 不得饱和后重复")
            .kind(),
        SkinErrorKind::RevisionExhausted
    );
    assert_eq!(store.load().expect("原设置应保持有效").revision, u64::MAX);
}

#[cfg(windows)]
#[test]
fn hardlinked_settings_file_is_rejected_without_reading_alias_content() {
    let (store, root) = fixture_store("hardlinked-json");
    let outside = root.join("outside.json");
    fs::write(&outside, persisted_json(3, valid_settings_json())).expect("应写入外部夹具");
    fs::hard_link(&outside, root.join("settings").join("skin.json")).expect("测试卷应支持硬链接");

    assert_eq!(
        store.load().expect_err("硬链接设置必须失败关闭").kind(),
        SkinErrorKind::FileSystem
    );
    assert_eq!(
        fs::read(&outside).expect("外部文件应保持可读"),
        persisted_json(3, valid_settings_json())
    );
}

#[cfg(windows)]
#[test]
fn reparse_settings_file_is_rejected_when_fixture_creation_is_allowed() {
    use std::os::windows::fs::symlink_file;

    let (store, root) = fixture_store("reparse-json");
    let outside = root.join("outside.json");
    fs::write(&outside, persisted_json(4, valid_settings_json())).expect("应写入外部夹具");
    if symlink_file(&outside, root.join("settings").join("skin.json")).is_err() {
        return;
    }

    assert_eq!(
        store.load().expect_err("reparse 设置必须失败关闭").kind(),
        SkinErrorKind::FileSystem
    );
}
