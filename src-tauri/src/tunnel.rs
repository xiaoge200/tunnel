use anyhow::{Context, Result};
use russh::client::{self, Handler};
use russh::*;
use russh_keys::load_secret_key;
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

// ─── Tunnel Runtime ───────────────────────────────────────────────────

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
            name: self.config.name.clone(),
            status,
            latency_ms: 0.0,
            rx_bytes_per_sec: 0.0,
            tx_bytes_per_sec: 0.0,
        };
        let _ = self.app_handle.emit("tunnel-metric", &metric);
    }

    pub async fn run(self: Arc<Self>) {
        if self.config.forwards.is_empty() {
            log::warn!("No forwards configured, nothing to do");
            return;
        }

        self.emit_status(TunnelStatus::Connecting);
        crate::set_tray_icon(&self.app_handle, TunnelStatus::Connecting);

        let ssh_addr = format!("{}:{}", self.config.ssh_host, self.config.ssh_port);

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
                crate::set_tray_icon(&self.app_handle, TunnelStatus::Error(format!("{:#}", e)));
                return;
            }
        };
        let handle = Arc::new(handle);

        // SSH disconnection watcher
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

        // Spawn one listener task per forward rule
        for fwd in &self.config.forwards {
            let task_handle = handle.clone();
            let task_stopped = self.stopped.clone();
            let task_rx = self.rx_bytes.clone();
            let task_tx = self.tx_bytes.clone();
            let target_host = fwd.target_host.clone();
            let target_port = fwd.target_port;
            let local_port = fwd.local_port;
            let name = self.config.name.clone();

            tokio::spawn(async move {
                run_forward_listener(
                    task_handle,
                    task_stopped,
                    task_rx,
                    task_tx,
                    &name,
                    local_port,
                    &target_host,
                    target_port,
                )
                .await;
            });
        }

        // ── Metrics emitter task (aggregated across all forwards) ────
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
                    name: metric_cfg.name.clone(),
                    status: TunnelStatus::Connected,
                    latency_ms: 0.0,
                    rx_bytes_per_sec: (current_rx.saturating_sub(prev_rx)) as f64,
                    tx_bytes_per_sec: (current_tx.saturating_sub(prev_tx)) as f64,
                };
                let _ = metric_ah.emit("tunnel-metric", &metric);
            }
        });

        log::info!(
            "Tunnel '{}' started with {} forward(s) via {}",
            self.config.name,
            self.config.forwards.len(),
            ssh_addr
        );
        self.emit_status(TunnelStatus::Connected);
        crate::set_tray_icon(&self.app_handle, TunnelStatus::Connected);

        // Wait for stop or disconnect
        tokio::select! {
            _ = self.wait_until_stopped() => {
                log::info!("Tunnel '{}' stopped by user", self.config.name);
            }
            _ = disconnect_notify.notified() => {
                log::warn!("Tunnel '{}' SSH connection lost", self.config.name);
            }
        }

        // Cleanup
        if self.stopped.load(Ordering::Relaxed) {
            self.emit_status(TunnelStatus::Disconnected);
            crate::set_tray_icon(&self.app_handle, TunnelStatus::Disconnected);
        } else {
            self.emit_status(TunnelStatus::Error("SSH 连接已断开".to_string()));
            crate::set_tray_icon(
                &self.app_handle,
                TunnelStatus::Error("SSH 连接已断开".to_string()),
            );
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

// ─── Per-forward listener ──────────────────────────────────────────────

async fn run_forward_listener(
    handle: Arc<client::Handle<SshClientHandler>>,
    stopped: Arc<AtomicBool>,
    rx_bytes: Arc<AtomicU64>,
    tx_bytes: Arc<AtomicU64>,
    tunnel_name: &str,
    local_port: u16,
    target_host: &str,
    target_port: u16,
) {
    let local_addr = format!("127.0.0.1:{}", local_port);
    let listener = match TcpListener::bind(&local_addr).await {
        Ok(l) => l,
        Err(e) => {
            log::error!(
                "Forward {}:{} bind failed: {:#}",
                tunnel_name,
                local_port,
                e
            );
            return;
        }
    };

    log::info!(
        "Forward {} listening on {} -> {}:{}",
        tunnel_name,
        local_addr,
        target_host,
        target_port
    );

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        let h = handle.clone();
                        let rx = rx_bytes.clone();
                        let tx = tx_bytes.clone();
                        let th = target_host.to_string();
                        tokio::spawn(async move {
                            if let Err(e) = forward_connection(h, &th, target_port, stream, rx, tx).await {
                                log::error!("Forward error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        log::error!("Accept error on port {}: {}", local_port, e);
                    }
                }
            }
            _ = wait_until(stopped.clone()) => {
                log::info!("Forward on port {} stopping", local_port);
                break;
            }
        }
    }
}

async fn wait_until(stopped: Arc<AtomicBool>) {
    loop {
        if stopped.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ─── Per-connection forwarding ───────────────────────────────────────────

async fn forward_connection(
    handle: Arc<client::Handle<SshClientHandler>>,
    target_host: &str,
    target_port: u16,
    mut local_stream: tokio::net::TcpStream,
    rx_bytes: Arc<AtomicU64>,
    tx_bytes: Arc<AtomicU64>,
) -> Result<()> {
    let mut channel = handle
        .channel_open_direct_tcpip(target_host, target_port as u32, "127.0.0.1", 0)
        .await
        .context("Failed to open direct-tcpip channel")?;

    let channel_id = channel.id();
    let mut local_buf = vec![0u8; 16384];

    loop {
        tokio::select! {
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

struct TunnelState {
    pub stopped: Arc<AtomicBool>,
}

pub struct TunnelManager {
    state: Arc<Mutex<Option<TunnelState>>>,
    config: Arc<Mutex<AppConfig>>,
    app_handle: AppHandle,
}

impl TunnelManager {
    pub fn new(app_handle: AppHandle, config: Arc<Mutex<AppConfig>>) -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
            config,
            app_handle,
        }
    }

    /// Start the tunnel — reads config from saved state.
    pub async fn start_tunnel(&self) {
        self.stop_inner().await;
        let cfg = match self.config.lock().await.tunnel.clone() {
            Some(c) => c,
            None => {
                log::warn!("No saved config to start");
                return;
            }
        };

        let runtime = Arc::new(TunnelRuntime::new(cfg, self.app_handle.clone()));
        let stopped = runtime.stopped.clone();
        let mgr_state = self.state.clone();

        tokio::spawn(async move {
            runtime.run().await;
            // 无论正常结束还是异常断线，都清除状态
            let mut s = mgr_state.lock().await;
            *s = None;
        });

        let mut state = self.state.lock().await;
        *state = Some(TunnelState { stopped });
    }

    /// Stop the tunnel.
    pub async fn stop_tunnel(&self) {
        self.stop_inner().await;
        // 立即通知前端，不等待 runtime cleanup 的异步事件
        let metric = TunnelMetric {
            name: String::new(),
            status: TunnelStatus::Disconnected,
            latency_ms: 0.0,
            rx_bytes_per_sec: 0.0,
            tx_bytes_per_sec: 0.0,
        };
        let _ = self.app_handle.emit("tunnel-metric", &metric);
    }

    pub async fn is_running(&self) -> bool {
        self.state.lock().await.is_some()
    }

    async fn stop_inner(&self) {
        let mut state = self.state.lock().await;
        if let Some(t) = state.take() {
            t.stopped.store(true, Ordering::Relaxed);
            log::info!("Tunnel stop signal sent");
        }
    }
}
