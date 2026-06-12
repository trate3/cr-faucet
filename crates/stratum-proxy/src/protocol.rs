//! Monero stratum-ish JSON-RPC message types.
//!
//! Reference: xmrig's `cryptonote` proxy protocol. Each message is a single
//! line of JSON terminated by `\n`. Methods are `login`, `submit`,
//! `keepalived`; the server pushes `job` notifications.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: serde_json::Value,
    pub jsonrpc: Option<String>,
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: serde_json::Value,
    pub jsonrpc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObj>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorObj {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginParams {
    pub login: String,         // miner EVM address
    pub pass: Option<String>,  // worker tag
    pub agent: Option<String>, // xmrig/version
    #[serde(default)]
    pub rigid: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub job_id: String,
    pub blob: String,    // hex
    pub target: String,  // hex, little-endian truncated target
    pub seed_hash: String,
    pub height: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitParams {
    pub id: String,      // login id assigned by server
    pub job_id: String,
    pub nonce: String,   // hex 4 bytes
    pub result: String,  // hex 32 bytes
}
