use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub job_id: String,
    pub stage: String,
    pub level: String,
    pub message: String,
    pub percent: u8,
}

impl ProgressEvent {
    // 统一构造事件，避免不同阶段输出字段不一致。
    pub fn new(
        job_id: impl Into<String>,
        stage: impl Into<String>,
        level: impl Into<String>,
        message: impl Into<String>,
        percent: u8,
    ) -> Self {
        Self {
            job_id: job_id.into(),
            stage: stage.into(),
            level: level.into(),
            message: message.into(),
            percent: percent.min(100),
        }
    }
}
