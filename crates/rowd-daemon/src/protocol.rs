use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DAEMON_IPC_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub version: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
