use anyhow::{Context, Result};
use russh::client::{self, Handler};
use russh::*;
use russh_keys::load_secret_key;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};

use crate::config::*;

const METRIC_EMIT_INTERVAL: Duration = Duration::from_secs(1);

// ─── Russh Client Handler ────────────────────────────────────────────

#[derive(Clone)]
struct SshClientHandler;

#[async_trait::async_trait]
impl Handler for SshClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh_keys::key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

// ─── Per-Tunnel Runtime ───────────────────────────────────────────────

struct TunnelRuntime {
    config: TunnelConfig,
    app_handle: AppHandle,
    stopped: Arc<AtomicBool>,
    rx_bytes: Arc<AtomicU64>,
    tx_bytes: Arc<AtomicU64>,
    last_rx: Arc<AtomicU64>,
    last_tx: Arc<AtomicU64>,
}

impl TunnelRuntime {
    fn new(config: TunnelConfig, app_handle: AppHandle) -> Self {
        Self {
            config,
            app_handle,
            stopped: Arc::new(AtomicBool::new(false)),
            rx_bytes: Arc::new(AtomicU64::new(0)),
            tx_bytes: Arc::new(AtomicU64::new(0)),
            last_rx: Arc::new(AtomicU64::new(0)),
            last_tx: Arc::new(AtomicU64::new(0)),
        }
    }

    fn emit_status(&self, status: TunnelStatus) {
        let metric = TunnelMetric {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            status,
            latency_ms: 0.0,
            rx_bytes_per_sec: 0.0,
            tx_bytes_per_sec: 0.0,
            local_port: self.config.local_port,
            target: format!("{}:{}", self.config.target_host, self.config.target_port),
        };
        let _ = self.app_handle.emit("tunnel-metric", &metric);
    }

    pub async fn run(self: Arc<Self>) {
        self.emit_status(TunnelStatus::Connecting);

        let ssh_addr = format!("{}:{}", self.config.ssh_host, self.config.ssh_port);
        let local_addr = format!("127.0.0.1:{}", self.config.local_port);

        // Build SSH session
        let handle = match self.build_session(&ssh_addr).await {
            Ok(h) => h,
            Err(e) => {
                log::error!(
                    "Tunnel '{}' SSH connection failed: {:#}",
                    self.config.name,
                    e
                );
                self.emit_status(TunnelStatus::Error(format!("{:#}", e)));
                return;
            }
        };
        let handle = Arc::new(handle);

        // SSH disconnection watcher — monitors Handle::is_closed()
        let disconnect_notify = Arc::new(Notify::new());
        let watcher_handle = handle.clone();
        let watcher_notify = disconnect_notify.clone();
        let watcher_stopped = self.stopped.clone();
        tokio::spawn(async move {
            loop {
                if watcher_stopped.load(Ordering::Relaxed) {
                    return;
                }
                if watcher_handle.is_closed() {
                    watcher_notify.notify_one();
                    return;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

        // Bind local TCP listener
        let listener = match TcpListener::bind(&local_addr).await {
            Ok(l) => l,
            Err(e) => {
                log::error!("Tunnel '{}' bind failed: {:#}", self.config.name, e);
                self.emit_status(TunnelStatus::Error(format!("{:#}", e)));
                return;
            }
        };

        log::info!(
            "Tunnel '{}' listening on {} -> {}:{} (via {})",
            self.config.name,
            local_addr,
            self.config.target_host,
            self.config.target_port,
            ssh_addr
        );
        self.emit_status(TunnelStatus::Connected);

        // ── Metrics emitter task ──────────────────────────────────────
        let metric_stopped = self.stopped.clone();
        let metric_cfg = self.config.clone();
        let metric_ah = self.app_handle.clone();
        let metric_rx = self.rx_bytes.clone();
        let metric_tx = self.tx_bytes.clone();
        let metric_last_rx = self.last_rx.clone();
        let metric_last_tx = self.last_tx.clone();
        tokio::spawn(async move {
            loop {
                if metric_stopped.load(Ordering::Relaxed) {
                    return;
                }
                tokio::time::sleep(METRIC_EMIT_INTERVAL).await;
                if metric_stopped.load(Ordering::Relaxed) {
                    return;
                }

                let current_rx = metric_rx.load(Ordering::Relaxed);
                let current_tx = metric_tx.load(Ordering::Relaxed);
                let prev_rx = metric_last_rx.swap(current_rx, Ordering::Relaxed);
                let prev_tx = metric_last_tx.swap(current_tx, Ordering::Relaxed);

                let metric = TunnelMetric {
                    id: metric_cfg.id.clone(),
                    name: metric_cfg.name.clone(),
                    status: TunnelStatus::Connected,
                    latency_ms: 0.0,
                    rx_bytes_per_sec: (current_rx.saturating_sub(prev_rx)) as f64,
                    tx_bytes_per_sec: (current_tx.saturating_sub(prev_tx)) as f64,
                    local_port: metric_cfg.local_port,
                    target: format!("{}:{}", metric_cfg.target_host, metric_cfg.target_port),
                };
                let _ = metric_ah.emit("tunnel-metric", &metric);
            }
        });

        // ── Accept loop ───────────────────────────────────────────────
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            let h = handle.clone();
                            let cfg = self.config.clone();
                            let rx = self.rx_bytes.clone();
                            let tx = self.tx_bytes.clone();
                            tokio::spawn(async move {
                                if let Err(e) = forward_connection(h, &cfg, stream, rx, tx).await {
                                    log::error!("Tunnel '{}' forward error: {:#}", cfg.name, e);
                                }
                            });
                        }
                        Err(e) => {
                            log::error!("Tunnel '{}' accept error: {}", self.config.name, e);
                        }
                    }
                }
                _ = self.wait_until_stopped() => {
                    log::info!("Tunnel '{}' stopped by user", self.config.name);
                    break;
                }
                _ = disconnect_notify.notified() => {
                    log::warn!(
                        "Tunnel '{}' SSH connection lost",
                        self.config.name
                    );
                    break;
                }
            }
        }

        // Cleanup — distinguish user stop from connection loss
        if self.stopped.load(Ordering::Relaxed) {
            self.emit_status(TunnelStatus::Disconnected);
        } else {
            self.emit_status(TunnelStatus::Error("SSH 连接已断开".to_string()));
        }
    }

    async fn wait_until_stopped(&self) {
        loop {
            if self.stopped.load(Ordering::Relaxed) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn build_session(&self, addr: &str) -> Result<client::Handle<SshClientHandler>> {
        let mut config = client::Config::default();
        // 保活配置：每 15s 发送心跳，连续 3 次无响应则判定断线（45s）
        config.keepalive_interval = Some(Duration::from_secs(15));
        config.keepalive_max = 3;
        let config = Arc::new(config);
        let handler = SshClientHandler;

        let mut handle = tokio::time::timeout(
            Duration::from_secs(15),
            client::connect(config, addr, handler),
        )
        .await
        .context("SSH connection timed out")?
        .context("SSH connection failed")?;

        let authenticated = match &self.config.auth_method {
            AuthMethod::Password { password } => handle
                .authenticate_password(&self.config.ssh_user, password)
                .await
                .context("Password authentication failed")?,
            AuthMethod::Key {
                private_key_path,
                passphrase,
            } => {
                let key = load_secret_key(private_key_path, passphrase.as_deref())
                    .context("Failed to load private key")?;
                handle
                    .authenticate_publickey(&self.config.ssh_user, Arc::new(key))
                    .await
                    .context("Public key authentication failed")?
            }
        };

        if !authenticated {
            anyhow::bail!("SSH authentication rejected");
        }

        Ok(handle)
    }
}

// ─── Per-connection forwarding ───────────────────────────────────────────

async fn forward_connection(
    handle: Arc<client::Handle<SshClientHandler>>,
    config: &TunnelConfig,
    mut local_stream: tokio::net::TcpStream,
    rx_bytes: Arc<AtomicU64>,
    tx_bytes: Arc<AtomicU64>,
) -> Result<()> {
    let mut channel = handle
        .channel_open_direct_tcpip(
            &config.target_host,
            config.target_port as u32,
            "127.0.0.1",
            0,
        )
        .await
        .context("Failed to open direct-tcpip channel")?;

    let channel_id = channel.id();
    let mut local_buf = vec![0u8; 16384];

    loop {
        tokio::select! {
            // Read from local stream, write to SSH channel
            result = local_stream.read(&mut local_buf) => {
                match result {
                    Ok(0) => {
                        let _ = channel.eof().await;
                        break;
                    }
                    Ok(n) => {
                        rx_bytes.fetch_add(n as u64, Ordering::Relaxed);
                        if handle.data(channel_id, CryptoVec::from_slice(&local_buf[..n])).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        log::error!("Local read error: {}", e);
                        break;
                    }
                }
            }
            // Read from SSH channel, write to local stream
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        tx_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                        if local_stream.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::Close) | None => break,
                    _ => continue,
                }
            }
        }
    }

    Ok(())
}

// ─── Tunnel Manager ───────────────────────────────────────────────────

struct RegisteredTunnel {
    pub stopped: Arc<AtomicBool>,
}

pub struct TunnelManager {
    tunnels: Arc<Mutex<HashMap<String, RegisteredTunnel>>>,
    app_handle: AppHandle,
}

impl TunnelManager {
    pub fn new(app_handle: AppHandle) -> Self {
        Self {
            tunnels: Arc::new(Mutex::new(HashMap::new())),
            app_handle,
        }
    }

    /// Start a new tunnel
    pub async fn start_tunnel(&self, config: TunnelConfig) {
        self.stop_tunnel_inner(&config.id).await;

        let runtime = Arc::new(TunnelRuntime::new(config, self.app_handle.clone()));
        let stopped = runtime.stopped.clone();

        let id = runtime.config.id.clone();
        tokio::spawn(async move {
            runtime.run().await;
        });

        let mut map = self.tunnels.lock().await;
        map.insert(id, RegisteredTunnel { stopped });
    }

    /// Stop a tunnel by id
    pub async fn stop_tunnel(&self, id: &str) {
        self.stop_tunnel_inner(id).await;
        let metric = TunnelMetric {
            id: id.to_string(),
            name: String::new(),
            status: TunnelStatus::Disconnected,
            latency_ms: 0.0,
            rx_bytes_per_sec: 0.0,
            tx_bytes_per_sec: 0.0,
            local_port: 0,
            target: String::new(),
        };
        let _ = self.app_handle.emit("tunnel-metric", &metric);
    }

    pub async fn stop_all_tunnels(&self) {
        for (id, _) in self.tunnels.lock().await.iter() {
            self.stop_tunnel_inner(id).await;
        }
    }

    async fn stop_tunnel_inner(&self, id: &str) {
        let mut map = self.tunnels.lock().await;
        if let Some(t) = map.remove(id) {
            t.stopped.store(true, Ordering::Relaxed);
            log::info!("Tunnel '{}' stop signal sent", id);
        }
    }
}
