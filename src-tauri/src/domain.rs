use semver::Version;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppPhase {
    #[default]
    Idle,
    Starting,
    Ready,
    Failed,
    Stopping,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    pub phase: AppPhase,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RuntimeEvent {
    Starting { message: String },
    Ready { url: String, elapsed_ms: u64 },
    Failed { code: String, message: String },
    Stopping { message: String },
}

/// 桌面端更新检查的用户可见结论。
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum UpdateNotice {
    UpToDate {
        current: Option<String>,
        official: String,
    },
    OfficialAvailable {
        current: Option<String>,
        official: String,
    },
    RuntimeAvailable {
        current: Option<String>,
        official: String,
        compatible: String,
    },
    DesktopRequired {
        current: Option<String>,
        official: String,
        compatible: String,
        minimum_desktop: String,
    },
    SkinUnverified {
        current: Option<String>,
        official: String,
        compatible: String,
    },
    Offline {
        current: Option<String>,
        version: Option<String>,
        error_kind: String,
    },
    CheckFailed {
        current: Option<String>,
        version: Option<String>,
        error_kind: String,
    },
}

/// 经过桌面更新后端验证的客户端发布记录。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopRelease {
    pub version: Version,
    pub notes: Option<String>,
    pub published_at: Option<String>,
}

/// 桌面更新只向状态文件与前端暴露的固定错误类别。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopUpdateErrorKind {
    Offline,
    InvalidMetadata,
    SignatureInvalid,
    InstallFailed,
}

/// 与 DSH runtime 更新完全隔离的桌面客户端更新状态。
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "phase",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DesktopUpdateState {
    #[default]
    Unavailable,
    Checking,
    UpToDate,
    Available {
        version: String,
        notes: Option<String>,
        published_at: Option<String>,
    },
    Downloading {
        version: String,
    },
    Installing {
        version: String,
    },
    Failed {
        error_kind: DesktopUpdateErrorKind,
    },
}

/// 桌面客户端更新状态的独立 revision 快照。
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopUpdateEnvelope {
    pub revision: u64,
    pub state: DesktopUpdateState,
}

#[cfg(test)]
mod tests {
    use super::{AppPhase, RuntimeEvent, RuntimeStatus, UpdateNotice};

    #[test]
    fn ready_event_uses_frontend_field_names() {
        let event = RuntimeEvent::Ready {
            url: "http://127.0.0.1:43127".to_owned(),
            elapsed_ms: 820,
        };
        let value = serde_json::to_value(event).expect("事件必须可序列化");
        assert_eq!(value["type"], "ready");
        assert_eq!(value["elapsedMs"], 820);
    }

    #[test]
    fn default_status_is_idle() {
        assert_eq!(RuntimeStatus::default().phase, AppPhase::Idle);
    }

    #[test]
    fn update_notice_variants_use_stable_snake_case_statuses() {
        let notices = [
            (
                UpdateNotice::OfficialAvailable {
                    current: None,
                    official: "0.1.1-rc.2".to_owned(),
                },
                "official_available",
            ),
            (
                UpdateNotice::RuntimeAvailable {
                    current: None,
                    official: "0.1.1-rc.2".to_owned(),
                    compatible: "0.1.1-rc.2".to_owned(),
                },
                "runtime_available",
            ),
            (
                UpdateNotice::DesktopRequired {
                    current: None,
                    official: "0.1.1-rc.2".to_owned(),
                    compatible: "0.1.1-rc.2".to_owned(),
                    minimum_desktop: "0.2.0".to_owned(),
                },
                "desktop_required",
            ),
            (
                UpdateNotice::SkinUnverified {
                    current: None,
                    official: "0.1.1-rc.2".to_owned(),
                    compatible: "0.1.1-rc.2".to_owned(),
                },
                "skin_unverified",
            ),
            (
                UpdateNotice::UpToDate {
                    current: Some("0.1.1-rc.2".to_owned()),
                    official: "0.1.1-rc.2".to_owned(),
                },
                "up_to_date",
            ),
            (
                UpdateNotice::Offline {
                    current: Some("0.1.1-rc.1".to_owned()),
                    version: None,
                    error_kind: "network".to_owned(),
                },
                "offline",
            ),
            (
                UpdateNotice::CheckFailed {
                    current: None,
                    version: None,
                    error_kind: "compatibility_verification".to_owned(),
                },
                "check_failed",
            ),
        ];

        for (notice, expected) in notices {
            let value = serde_json::to_value(notice).expect("更新结论必须可序列化");
            assert_eq!(value["status"], expected);
        }
    }
}
