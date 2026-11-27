use serde::{Deserialize, Serialize};

use super::{DaemonMessage, DaemonRequest};

pub const RELAY_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayUser {
    pub user_id: String,
    pub username: String,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayDeviceInfo {
    pub device_id: String,
    pub name: String,
    pub os: String,
    pub arch: String,
    pub online: bool,
    pub last_seen_ms: u64,
    #[serde(default)]
    pub workspaces: Vec<String>,
    #[serde(default)]
    pub daemon_protocol: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayAuthResponse {
    pub user: RelayUser,
    pub session_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCodeStartResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in_secs: u64,
    pub interval_secs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeviceCodePollResponse {
    Pending,
    Expired,
    Approved {
        user: RelayUser,
        session_token: String,
        device_id: String,
        device_token: String,
    },
}

/// Reserved metadata slot for future end-to-end encryption between clients and
/// `pig-daemon` without breaking the relay framing protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayE2eHeader {
    pub scheme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
}

/// Frames sent from `pig-daemon` to `pig-relay` over the outbound tunnel.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RelayDaemonFrame {
    Register {
        protocol: u32,
        device_id: String,
        name: String,
        os: String,
        arch: String,
        #[serde(default)]
        workspaces: Vec<String>,
        daemon_protocol: u32,
    },
    Heartbeat {
        #[serde(default)]
        workspaces: Vec<String>,
    },
    ClientMessage {
        stream_id: String,
        message: DaemonMessage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        e2e: Option<RelayE2eHeader>,
    },
    ClientClosed {
        stream_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// Frames sent from `pig-relay` to `pig-daemon` over the outbound tunnel.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RelayServerFrame {
    Registered {
        device_id: String,
        server_time_ms: u64,
    },
    ClientConnected {
        stream_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_label: Option<String>,
    },
    ClientRequest {
        stream_id: String,
        request: DaemonRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        e2e: Option<RelayE2eHeader>,
    },
    ClientDisconnected {
        stream_id: String,
    },
    Error {
        detail: String,
    },
}

/// Local status returned by `pig-daemon`'s `relay.status` RPC.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayConnectionStatus {
    pub configured: bool,
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}
