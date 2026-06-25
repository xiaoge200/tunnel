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

    /// 核心入口：包含自动重连循环。
    /// SSH 意外断开时会自动清理旧会话、等待、重试，
    /// 直到用户主动停止。
    pub async fn run(self: Arc<Self>) {
        if self.config.forwards.is_empty() {
            log::warn!("No forwards configured, nothing to do");
            return;
        }

        let ssh_addr = format!("{}:{}", self.config.ssh_host, self.config.ssh_port);

        // ─── 外层重连循环 ──────────────────────────────────────────
        'retry: loop {
            if self.stopped.load(Ordering::Relaxed) {
                break;
            }

            // ── 连接阶段 ────────────────────────────────────────────
            self.emit_status(TunnelStatus::Connecting);
            crate::set_tray_icon(&self.app_handle, TunnelStatus::Connecting);

            let handle = match self.build_session(&ssh_addr).await {
                Ok(h) => Arc::new(h),
                Err(e) => {
                    log::error!(
                        "Tunnel '{}' SSH connection failed: {:#}",
                        self.config.name,
                        e
                    );
                    self.sleep_with_stop_check(Duration::from_secs(5)).await;
                    continue;
                }
            };

            // ── 会话级取消标记：断开重连时终止旧监听器 ────────────
            let session_cancel = Arc::new(AtomicBool::new(false));

            // ── SSH 断线检测 ───────────────────────────────────────
            let disconnect_notify = Arc::new(Notify::new());
            let watcher_handle = handle.clone();
            let watcher_notify = disconnect_notify.clone();
            let watcher_stopped = self.stopped.clone();
            let watcher_cancel = session_cancel.clone();
            tokio::spawn(async move {
                loop {
                    if watcher_stopped.load(Ordering::Relaxed)
                        || watcher_cancel.load(Ordering::Relaxed)
                    {
                        return;
                    }
                    if watcher_handle.is_closed() {
                        watcher_notify.notify_one();
                        return;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            });

            // ── 重置计数器，每次新会话从零开始 ─────────────────────
            self.rx_bytes.store(0, Ordering::Relaxed);
            self.tx_bytes.store(0, Ordering::Relaxed);
            self.last_rx.store(0, Ordering::Relaxed);
            self.last_tx.store(0, Ordering::Relaxed);

            // ── 先绑定所有端口（同步），任何一个失败就重试 ─────────
            //     这样可以避免旧监听器尚未释放端口时静默失败的问题。
            let mut bound_listeners: Vec<(ForwardRule, TcpListener)> = Vec::new();
            let mut bind_ok = true;

            for fwd in &self.config.forwards {
                match bind_local_port(fwd.local_port).await {
                    Ok(listener) => {
                        bound_listeners.push((fwd.clone(), listener));
                    }
                    Err(e) => {
                        log::error!(
                            "Tunnel '{}' port {} bind failed: {}",
                            self.config.name,
                            fwd.local_port,
                            e
                        );
                        bind_ok = false;
                        break;
                    }
                }
            }

            if !bind_ok {
                // 释放已经绑定的端口，确保下次重试时干净
                drop(bound_listeners);
                // SSH 会话也用不上了，放弃
                drop(handle);
                // 通知前端，让用户看到连接失败
                self.emit_status(TunnelStatus::Connecting);
                crate::set_tray_icon(&self.app_handle, TunnelStatus::Connecting);
                // 等一会儿再重试，给旧监听器足够时间释放端口
                self.sleep_with_stop_check(Duration::from_secs(3)).await;
                continue 'retry;
            }

            // ── 端口全部绑定成功，启动前向监听器任务 ───────────────
            for (fwd, listener) in bound_listeners {
                let task_handle = handle.clone();
                let task_stopped = self.stopped.clone();
                let task_cancel = session_cancel.clone();
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
                        task_cancel,
                        task_rx,
                        task_tx,
                        &name,
                        local_port,
                        listener,
                        &target_host,
                        target_port,
                    )
                    .await;
                });
            }

            // ── 指标发射器 ─────────────────────────────────────────
            let metric_stopped = self.stopped.clone();
            let metric_cancel = session_cancel.clone();
            let metric_cfg = self.config.clone();
            let metric_ah = self.app_handle.clone();
            let metric_rx = self.rx_bytes.clone();
            let metric_tx = self.tx_bytes.clone();
            let metric_last_rx = self.last_rx.clone();
            let metric_last_tx = self.last_tx.clone();
            tokio::spawn(async move {
                loop {
                    if metric_stopped.load(Ordering::Relaxed)
                        || metric_cancel.load(Ordering::Relaxed)
                    {
                        return;
                    }
                    tokio::time::sleep(METRIC_EMIT_INTERVAL).await;
                    if metric_stopped.load(Ordering::Relaxed)
                        || metric_cancel.load(Ordering::Relaxed)
                    {
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

            // ── 等待用户停止或 SSH 断线 ───────────────────────────
            let is_disconnected = {
                tokio::select! {
                    _ = self.wait_until_stopped() => {
                        log::info!("Tunnel '{}' stopped by user", self.config.name);
                        false
                    }
                    _ = disconnect_notify.notified() => {
                        log::warn!("Tunnel '{}' SSH connection lost, reconnecting...", self.config.name);
                        true
                    }
                }
            };

            // ── 清理旧会话（通知所有子任务退出） ───────────────────
            session_cancel.store(true, Ordering::Relaxed);

            if !is_disconnected {
                break; // 用户主动停止，退出外层重连循环
            }

            // 断开重连前等几秒，避免频繁重试
            self.sleep_with_stop_check(Duration::from_secs(5)).await;
        }

        // ── 最终清理 ───────────────────────────────────────────────
        self.emit_status(TunnelStatus::Disconnected);
        crate::set_tray_icon(&self.app_handle, TunnelStatus::Disconnected);
    }

    /// 可被停止信号打断的 sleep：让停止响应更及时
    async fn sleep_with_stop_check(&self, duration: Duration) {
        let step = Duration::from_millis(500);
        let mut elapsed = Duration::ZERO;
        while elapsed < duration {
            if self.stopped.load(Ordering::Relaxed) {
                return;
            }
            tokio::time::sleep(step).await;
            elapsed += step;
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

// ─── 端口绑定辅助函数 ──────────────────────────────────────────────────

/// 绑定本地端口，带 SO_REUSEADDR 和重试逻辑。
/// 确保休眠唤醒后旧监听器尚未完全释放端口时也能尽快重新绑定。
async fn bind_local_port(port: u16) -> std::io::Result<TcpListener> {
    let addr = format!("127.0.0.1:{}", port);
    let sock_addr: std::net::SocketAddr = addr
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    for attempt in 0..5 {
        // 用 socket2 创建 socket 并设置 SO_REUSEADDR，
        // 这样旧 socket 关闭后可以立刻重用端口
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;

        // SO_REUSEADDR：允许快速重用 TIME_WAIT / 刚关闭的端口
        let _ = socket.set_reuse_address(true);

        let sa = socket2::SockAddr::from(sock_addr);
        if let Err(e) = socket.bind(&sa) {
            drop(socket);
            if attempt < 4 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            return Err(e);
        }

        socket.listen(128)?;

        // 将 std 的 TcpListener 转换为 tokio 的 TcpListener
        let std_listener: std::net::TcpListener = socket.into();
        return TcpListener::from_std(std_listener);
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        format!("port {} bind failed after retries", port),
    ))
}

// ─── Per-forward listener ──────────────────────────────────────────────

async fn run_forward_listener(
    handle: Arc<client::Handle<SshClientHandler>>,
    stopped: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    rx_bytes: Arc<AtomicU64>,
    tx_bytes: Arc<AtomicU64>,
    tunnel_name: &str,
    local_port: u16,
    listener: TcpListener,
    target_host: &str,
    target_port: u16,
) {
    log::info!(
        "Forward {} listening on 127.0.0.1:{} -> {}:{}",
        tunnel_name,
        local_port,
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
            _ = wait_flag(stopped.clone()) => {
                log::info!("Forward on port {} stopped by user", local_port);
                break;
            }
            _ = wait_flag(cancel.clone()) => {
                log::info!("Forward on port {} cancelled by session reset", local_port);
                break;
            }
        }
    }
}

async fn wait_flag(flag: Arc<AtomicBool>) {
    loop {
        if flag.load(Ordering::Relaxed) {
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
