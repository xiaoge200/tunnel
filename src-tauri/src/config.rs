use serde::{Deserialize, Serialize};

// ─── Encrypted password storage ──────────────────────────────────────

mod secret {
    use base64::Engine;
    use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
    use ring::rand::{SecureRandom, SystemRandom};

    const KEY: &[u8; 32] = b"TnlApp_256bit_EncryptionKey__!!!";

    pub fn encrypt(plain: &str) -> String {
        let rng = SystemRandom::new();
        let unbound = UnboundKey::new(&AES_256_GCM, KEY).expect("valid key");
        let key = LessSafeKey::new(unbound);

        let mut nonce = [0u8; 12];
        rng.fill(&mut nonce).expect("rng");

        let mut buf = plain.as_bytes().to_vec();
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
            .expect("seal");

        let mut out = nonce.to_vec();
        out.extend_from_slice(&buf);
        base64::engine::general_purpose::STANDARD.encode(&out)
    }

    pub fn decrypt(encoded: &str) -> Option<String> {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?;
        if data.len() < 28 {
            return None;
        }

        let (nonce, ct) = data.split_at(12);
        let unbound = UnboundKey::new(&AES_256_GCM, KEY).expect("valid key");
        let key = LessSafeKey::new(unbound);

        let mut buf = ct.to_vec();
        let plain = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce.try_into().ok()?),
                Aad::empty(),
                &mut buf,
            )
            .ok()?;
        String::from_utf8(plain.to_vec()).ok()
    }
}

use secret as sec;

/// A single port forwarding rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardRule {
    pub local_port: u16,
    pub target_host: String,
    pub target_port: u16,
}

/// SSH tunnel configuration with multiple port forwards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelConfig {
    pub name: String,
    pub ssh_host: String,
    pub ssh_port: u16,
    pub ssh_user: String,
    pub auth_method: AuthMethod,
    pub forwards: Vec<ForwardRule>,
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
            name: "SSH 隧道".into(),
            ssh_host: "127.0.0.1".into(),
            ssh_port: 22,
            ssh_user: "root".into(),
            auth_method: AuthMethod::Password {
                password: "".into(),
            },
            forwards: vec![ForwardRule {
                local_port: 15432,
                target_host: "127.0.0.1".into(),
                target_port: 15432,
            }],
        }
    }
}

/// Tunnel runtime status emitted to frontend every 1s.
#[derive(Debug, Clone, Serialize)]
pub struct TunnelMetric {
    pub name: String,
    pub status: TunnelStatus,
    pub latency_ms: f64,
    pub rx_bytes_per_sec: f64,
    pub tx_bytes_per_sec: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum TunnelStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

/// Persisted config — single tunnel or none.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub tunnel: Option<TunnelConfig>,
}

impl AppConfig {
    pub fn defaults() -> Self {
        Self {
            tunnel: Some(TunnelConfig::default()),
        }
    }

    /// 写入文件前加密所有密码
    pub fn encrypt_passwords(&mut self) {
        if let Some(ref mut t) = self.tunnel {
            if let AuthMethod::Password { ref mut password } = t.auth_method {
                if !password.is_empty() && !looks_encrypted(password) {
                    *password = sec::encrypt(password);
                }
            }
        }
    }

    /// 从文件读取后解密所有密码
    pub fn decrypt_passwords(&mut self) {
        if let Some(ref mut t) = self.tunnel {
            if let AuthMethod::Password { ref mut password } = t.auth_method {
                if !password.is_empty() {
                    if let Some(plain) = sec::decrypt(password) {
                        *password = plain;
                    }
                }
            }
        }
    }
}

fn looks_encrypted(s: &str) -> bool {
    s.len() > 40
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
}
