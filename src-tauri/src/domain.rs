use serde::Serialize;

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

#[cfg(test)]
mod tests {
    use super::{AppPhase, RuntimeEvent, RuntimeStatus};

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
}
