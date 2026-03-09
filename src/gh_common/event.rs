use serde::{Deserialize, Serialize};

pub use crate::gh_common::model::SessionEvent;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GhEvent {
    Session {
        session_id: String,
        event: SessionEvent,
    },
    InferenceJson {
        session_id: String,
        payload_json: String,
    },
    Diagnostics {
        session_id: String,
        message: String,
    },
}
