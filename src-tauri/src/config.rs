use serde::{Deserialize, Serialize};

/// Represents a single SSH tunnel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelConfig {
    pub id: String,
    pub name: String,
    pub ssh_host: String,
    pub ssh_port: u16,
    pub ssh_user: String,
    pub auth_method: AuthMethod,
    pub local_port: u16,
    pub target_host: String,
    pub target_port: u16,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthMethod {
    Password {
        password: String,
    },
    Key {
        private_key_path: String,
        passphrase: Option<String>,
    },
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: "New Tunnel".into(),
            ssh_host: "127.0.0.1".into(),
            ssh_port: 22,
            ssh_user: "root".into(),
            auth_method: AuthMethod::Password {
                password: "".into(),
            },
            local_port: 15432,
            target_host: "127.0.0.1".into(),
            target_port: 5432,
            enabled: false,
        }
    }
}

/// Tunnel runtime status emitted to frontend every 1s.
#[derive(Debug, Clone, Serialize)]
pub struct TunnelMetric {
    pub id: String,
    pub name: String,
    pub status: TunnelStatus,
    pub latency_ms: f64,
    pub rx_bytes_per_sec: f64,
    pub tx_bytes_per_sec: f64,
    pub local_port: u16,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum TunnelStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

/// Persisted config collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub tunnels: Vec<TunnelConfig>,
}

impl AppConfig {
    pub fn defaults() -> Self {
        Self {
            tunnels: vec![TunnelConfig {
                id: uuid::Uuid::new_v4().to_string(),
                name: "新建隧道".into(),
                ssh_host: "127.0.0.1".into(),
                ssh_port: 22,
                ssh_user: "root".into(),
                auth_method: AuthMethod::Password {
                    password: "".into(),
                },
                local_port: 5432,
                target_host: "127.0.0.1".into(),
                target_port: 5432,
                enabled: false,
            }],
        }
    }
}
