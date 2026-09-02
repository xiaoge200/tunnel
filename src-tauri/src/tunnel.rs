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

// ─── Tunnel Runtime ───────────────────────────────────────────────────

type RunningMap = Mutex<HashMap<String, RunningTunnel>>;

/// 状态发布唯一通道(与 manager 共享 running 表):更新状态表 →
/// emit 事件 → 刷新托盘。1Hz 指标 tick 不走这里(见 run),防止每秒
/// 重建托盘菜单。调用方不得持有 config/running 锁。
async fn publish_metric(running: &Arc<RunningMap>, app: &AppHandle, metric: &TunnelMetric) {
    {
        let mut map = running.lock().await;
        if let Some(e) = map.get_mut(&metric.id) {
            e.status = metric.status.clone();
            e.name = metric.name.clone();
        }
    }
    let _ = app.emit("tunnel-metric", metric);
    let _ = crate::refresh_tray(app).await;
}

/// runtime 退出后摘除注册;用 stopped 指针校验,过期 runtime 不会
/// 误删同 id 的新 runtime。调用方不得持有 running 锁。
async fn unregister_runtime(
    running: &Arc<RunningMap>,
    app: &AppHandle,
    id: &str,
    stopped: &Arc<AtomicBool>,
) {
    {
        let mut map = running.lock().await;
        if let Some(e) = map.get(id) {
            if Arc::ptr_eq(&e.stopped, stopped) {
                map.remove(id);
            }
        }
    }
    let _ = crate::refresh_tray(app).await;
}

/// 一次运行尝试:一条 SSH 连接 + 全部转发监听。
/// 结束(用户停止 / SSH 断开 / 连接失败)时先发布终态,再由 runner 任务
/// 调 `unregister_runtime` 摘除注册。
struct TunnelRuntime {
    config: TunnelConfig,
    app_handle: AppHandle,
    running: Arc<RunningMap>,
    stopped: Arc<AtomicBool>,
    rx_bytes: Arc<AtomicU64>,
    tx_bytes: Arc<AtomicU64>,
    last_rx: Arc<AtomicU64>,
    last_tx: Arc<AtomicU64>,
}

impl TunnelRuntime {
    fn new(config: TunnelConfig, app_handle: AppHandle, running: Arc<RunningMap>) -> Self {
        Self {
            config,
            app_handle,
            running,
            stopped: Arc::new(AtomicBool::new(false)),
            rx_bytes: Arc::new(AtomicU64::new(0)),
            tx_bytes: Arc::new(AtomicU64::new(0)),
            last_rx: Arc::new(AtomicU64::new(0)),
            last_tx: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 状态转换统一走 publish(更新状态表 → emit 事件 → 刷新托盘),
    /// 前端与托盘从同一事实源取状态。1Hz 指标 tick 不走这里(见 run)。
    async fn publish_status(&self, status: TunnelStatus) {
        let metric = TunnelMetric {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            status,
            rx_bytes_per_sec: 0.0,
            tx_bytes_per_sec: 0.0,
        };
        publish_metric(&self.running, &self.app_handle, &metric).await;
    }

    pub async fn run(self: Arc<Self>) {
        if self.config.forwards.is_empty() {
            log::warn!(
                "Tunnel '{}' has no forwards configured, nothing to do",
                self.config.name
            );
            self.publish_status(TunnelStatus::Disconnected).await;
            return;
        }

        self.publish_status(TunnelStatus::Connecting).await;

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
                // 用户在连接期间点了停止:按正常断开处理,不闪红
                if self.stopped.load(Ordering::Relaxed) {
                    self.publish_status(TunnelStatus::Disconnected).await;
                } else {
                    self.publish_status(TunnelStatus::Error(format!("{:#}", e))).await;
                }
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
        // 直接 emit,不走 publish —— 否则每秒都会重建一次托盘菜单。
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
        self.publish_status(TunnelStatus::Connected).await;

        // Wait for stop or disconnect
        tokio::select! {
            _ = self.wait_until_stopped() => {
                log::info!("Tunnel '{}' stopped by user", self.config.name);
            }
            _ = disconnect_notify.notified() => {
                log::warn!("Tunnel '{}' SSH connection lost", self.config.name);
            }
        }

        // Cleanup — 终态必须先发布(前端/托盘据此复位),之后 runner 任务才会
        // 调 remove_runtime 摘除注册。
        if self.stopped.load(Ordering::Relaxed) {
            self.publish_status(TunnelStatus::Disconnected).await;
        } else {
            self.publish_status(TunnelStatus::Error("SSH 连接已断开".to_string()))
                .await;
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

/// 一个运行中(或正在拆除)runtime 的注册条目,key 为隧道 id。
struct RunningTunnel {
    stopped: Arc<AtomicBool>,
    /// 最近一次状态转换;stopped=true 表示拆除中,不再视为"运行中"。
    status: TunnelStatus,
    /// 启动时的名称快照(托盘/快照用),随配置改名由 refresh_names 同步。
    name: String,
}

/// 多隧道注册表:每个 id 至多一个 runtime。
/// 锁纪律:任何路径先取 config 锁、后取 running 锁,锁内只克隆数据、
/// 绝不跨锁 await GUI(托盘刷新);config 锁释放后才可调 prune/stop。
pub struct TunnelManager {
    running: Arc<RunningMap>,
    config: Arc<Mutex<AppConfig>>,
    app_handle: AppHandle,
}

impl TunnelManager {
    pub fn new(app_handle: AppHandle, config: Arc<Mutex<AppConfig>>) -> Self {
        Self {
            running: Arc::new(Mutex::new(HashMap::new())),
            config,
            app_handle,
        }
    }

    /// 启动指定隧道(id 必须已保存在配置中,且至少一条转发规则)。
    /// 已在运行 → Err;上一个 runtime 仍在拆除 → 等它退干净(~3s 上限)
    /// 再注册,防止两个 runtime 抢占同一批本地端口。
    pub async fn start_tunnel(&self, id: &str) -> Result<(), String> {
        let cfg = {
            let guard = self.config.lock().await;
            guard.tunnels.iter().find(|t| t.id == id).cloned()
        }
        .ok_or_else(|| format!("未找到隧道: {}", id))?;
        if cfg.forwards.is_empty() {
            return Err(format!("隧道 '{}' 没有转发规则", cfg.name));
        }

        let mut wait_ticks = 0u32;
        let stopped = loop {
            {
                let mut running = self.running.lock().await;
                match running.get(id) {
                    None => {
                        let stopped = Arc::new(AtomicBool::new(false));
                        running.insert(
                            id.to_string(),
                            RunningTunnel {
                                stopped: stopped.clone(),
                                status: TunnelStatus::Connecting,
                                name: cfg.name.clone(),
                            },
                        );
                        break stopped;
                    }
                    Some(e) if !e.stopped.load(Ordering::Relaxed) => {
                        return Err(format!("隧道 '{}' 已在运行", cfg.name));
                    }
                    // Some + stopped=true:上一个 runtime 正在拆除,继续等待
                    Some(_) => {}
                }
            }
            if wait_ticks >= 60 {
                return Err(format!("隧道 '{}' 仍在停止中,请稍后重试", cfg.name));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            wait_ticks += 1;
        };

        let runtime = Arc::new(TunnelRuntime::new(
            cfg,
            self.app_handle.clone(),
            self.running.clone(),
        ));
        let running = self.running.clone();
        let app_handle = self.app_handle.clone();
        let id = id.to_string();
        let cleanup_stopped = stopped.clone();
        tokio::spawn(async move {
            runtime.run().await;
            // 终态已由 run() 发布,这里只负责摘除注册
            unregister_runtime(&running, &app_handle, &id, &cleanup_stopped).await;
        });
        Ok(())
    }

    /// 请求停止:只置位标志;终态(Disconnected)由 runtime 清理路径
    /// 统一发布,保证状态权威唯一。
    pub async fn stop_tunnel(&self, id: &str) {
        let running = self.running.lock().await;
        if let Some(e) = running.get(id) {
            e.stopped.store(true, Ordering::Relaxed);
        }
    }

    /// 启动所有未在运行的隧道;单台失败只记日志,不阻塞其余。
    pub async fn start_all(&self) {
        let ids: Vec<String> = {
            let cfg = self.config.lock().await;
            cfg.tunnels.iter().map(|t| t.id.clone()).collect()
        };
        for id in ids {
            if let Err(e) = self.start_tunnel(&id).await {
                log::warn!("全部连接:隧道启动失败: {}", e);
            }
        }
    }

    /// 停止所有运行中隧道。
    pub async fn stop_all(&self) {
        let running = self.running.lock().await;
        for e in running.values() {
            e.stopped.store(true, Ordering::Relaxed);
        }
    }

    pub async fn is_running(&self, id: &str) -> bool {
        let running = self.running.lock().await;
        matches!(running.get(id), Some(e) if !e.stopped.load(Ordering::Relaxed))
    }

    /// 运行中隧道的状态表快照(托盘汇总图标用)。
    pub async fn active_statuses(&self) -> HashMap<String, TunnelStatus> {
        let running = self.running.lock().await;
        running
            .iter()
            .filter(|(_, e)| !e.stopped.load(Ordering::Relaxed))
            .map(|(id, e)| (id.clone(), e.status.clone()))
            .collect()
    }

    /// 供 `get_tunnel_statuses` 命令:新开窗口用它做状态初始化
    /// (事件是瞬时的,只靠 listen 会漏掉转换瞬间)。
    pub async fn status_metrics(&self) -> Vec<TunnelMetric> {
        let running = self.running.lock().await;
        running
            .iter()
            .filter(|(_, e)| !e.stopped.load(Ordering::Relaxed))
            .map(|(id, e)| TunnelMetric {
                id: id.clone(),
                name: e.name.clone(),
                status: e.status.clone(),
                rx_bytes_per_sec: 0.0,
                tx_bytes_per_sec: 0.0,
            })
            .collect()
    }

    /// 保存配置后调用:停止已不在新列表中的运行中隧道(删除安全)。
    pub async fn prune(&self, keep_ids: &[String]) {
        let ids: Vec<String> = {
            let running = self.running.lock().await;
            running
                .keys()
                .filter(|id| !keep_ids.contains(id))
                .cloned()
                .collect()
        };
        for id in ids {
            self.stop_tunnel(&id).await;
        }
    }

    /// 运行中隧道的名称快照随配置改名同步(托盘标签用)。
    pub async fn refresh_names(&self, tunnels: &[TunnelConfig]) {
        let mut running = self.running.lock().await;
        for (id, e) in running.iter_mut() {
            if let Some(t) = tunnels.iter().find(|t| &t.id == id) {
                e.name = t.name.clone();
            }
        }
    }
}
