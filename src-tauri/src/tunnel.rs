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
/// 连接失败 / 断开后的重试间隔(可被停止信号打断)。
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// 端口绑定失败后的单次等待与重试上限。
const BIND_RETRY_DELAY: Duration = Duration::from_millis(500);
const BIND_RETRY_MAX: u32 = 5;

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

/// 绑定本地端口,带 5×500ms 重试(休眠唤醒后旧监听器释放端口有延迟)。
///
/// 注意:Windows 上**不能**开 SO_REUSEADDR —— 它会让多个 socket 同时
/// 绑上同一端口,新连接被随机分发到已失效的旧监听器(表现为"隧道无效")。
/// 只能靠重试等旧监听器自行退出。
async fn bind_local_port(port: u16) -> std::io::Result<TcpListener> {
    let addr = format!("127.0.0.1:{}", port);
    let mut last_err = None;
    for attempt in 0..BIND_RETRY_MAX {
        match TcpListener::bind(&addr).await {
            Ok(l) => return Ok(l),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < BIND_RETRY_MAX {
                    tokio::time::sleep(BIND_RETRY_DELAY).await;
                }
            }
        }
    }
    Err(last_err.unwrap())
}

/// 一次运行:包含外层自动重连循环。
/// SSH 连接失败或意外断开后,在 RETRY_DELAY 后自动重连,**直到用户停止**;
/// 因此 runtime 只在用户停止时退出(由 runner 任务摘除注册)。
/// 连接失败会发布带原因的 Error(前端卡片内联显示),随后继续后台重试。
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

        let ssh_addr = format!("{}:{}", self.config.ssh_host, self.config.ssh_port);

        // ─── 外层自动重连循环 ──────────────────────────────────────
        'retry: loop {
            if self.stopped.load(Ordering::Relaxed) {
                break;
            }

            // ── 连接阶段 ────────────────────────────────────────────
            self.publish_status(TunnelStatus::Connecting).await;
            let handle = match self.build_session(&ssh_addr).await {
                Ok(h) => h,
                Err(e) => {
                    log::error!(
                        "Tunnel '{}' SSH connection failed: {:#}",
                        self.config.name,
                        e
                    );
                    if self.stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    // 带原因发布一次错误(卡片内联显示),随后后台自动重试
                    self.publish_status(TunnelStatus::Error(format!("{:#}", e)))
                        .await;
                    self.sleep_interruptible(RETRY_DELAY).await;
                    continue;
                }
            };

            // 连接期间用户点了停止:关掉刚建好的会话
            if self.stopped.load(Ordering::Relaxed) {
                let _ = handle
                    .disconnect(Disconnect::ByApplication, "user stopped", "")
                    .await;
                break;
            }

            // ── 先绑定全部本地端口:任一失败 → 整体放弃本次会话重试 ──
            //     (避免"SSH 已连但某转发静默失效"的半工作隧道)
            let mut bound: Vec<(ForwardRule, TcpListener)> = Vec::new();
            let mut bind_ok = true;
            for fwd in &self.config.forwards {
                match bind_local_port(fwd.local_port).await {
                    Ok(listener) => bound.push((fwd.clone(), listener)),
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
                drop(bound);
                // 会话尚未交给任何子任务,这里主动断开避免连接泄漏
                let _ = handle
                    .disconnect(Disconnect::ByApplication, "port bind failed", "")
                    .await;
                if self.stopped.load(Ordering::Relaxed) {
                    break;
                }
                self.publish_status(TunnelStatus::Connecting).await;
                self.sleep_interruptible(Duration::from_secs(3)).await;
                continue 'retry;
            }

            let handle = Arc::new(handle);

            // ── 会话级取消标记:断开重连时终止旧会话的全部子任务 ────
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

            // 新会话计数器从零开始,避免跨会话产生速率尖峰
            self.rx_bytes.store(0, Ordering::Relaxed);
            self.tx_bytes.store(0, Ordering::Relaxed);
            self.last_rx.store(0, Ordering::Relaxed);
            self.last_tx.store(0, Ordering::Relaxed);

            // ── 前向监听任务(使用已绑定的 listener) ──────────────
            for (fwd, listener) in bound {
                let task_handle = handle.clone();
                let task_stopped = self.stopped.clone();
                let task_cancel = session_cancel.clone();
                let task_rx = self.rx_bytes.clone();
                let task_tx = self.tx_bytes.clone();
                let name = self.config.name.clone();
                tokio::spawn(async move {
                    run_forward_listener(
                        task_handle,
                        task_stopped,
                        task_cancel,
                        task_rx,
                        task_tx,
                        &name,
                        fwd,
                        listener,
                    )
                    .await;
                });
            }

            // ── Metrics emitter task (aggregated across all forwards) ────
            // 直接 emit,不走 publish —— 否则每秒都会重建一次托盘菜单。
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

            // ── 等待用户停止或 SSH 断线 ───────────────────────────
            let disconnected = tokio::select! {
                _ = self.wait_until_stopped() => {
                    log::info!("Tunnel '{}' stopped by user", self.config.name);
                    false
                }
                _ = disconnect_notify.notified() => {
                    log::warn!(
                        "Tunnel '{}' SSH connection lost, reconnecting...",
                        self.config.name
                    );
                    true
                }
            };

            // ── 清理旧会话(终止全部子任务,释放端口) ───────────────
            session_cancel.store(true, Ordering::Relaxed);

            if !disconnected {
                break; // 用户主动停止 → 退出外层重连循环
            }

            // 断开后等一会儿再重连,避免频繁重试
            self.sleep_interruptible(RETRY_DELAY).await;
        }

        // 只有用户停止才会走到这里(断开是自动重连,不退出)
        self.publish_status(TunnelStatus::Disconnected).await;
    }

    /// 可被停止信号打断的 sleep:让停止响应更及时(500ms 步进)。
    async fn sleep_interruptible(&self, total: Duration) {
        let mut waited = Duration::ZERO;
        while waited < total {
            if self.stopped.load(Ordering::Relaxed) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            waited += Duration::from_millis(500);
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

/// 在已绑定的 listener 上接收连接并转发。
/// stopped / cancel 任一置位即退出(用户停止 / 会话重建)。
async fn run_forward_listener(
    handle: Arc<client::Handle<SshClientHandler>>,
    stopped: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    rx_bytes: Arc<AtomicU64>,
    tx_bytes: Arc<AtomicU64>,
    tunnel_name: &str,
    fwd: ForwardRule,
    listener: TcpListener,
) {
    log::info!(
        "Forward {} listening on {} -> {}:{}",
        tunnel_name,
        listener.local_addr().map(|a| a.to_string()).unwrap_or_default(),
        fwd.target_host,
        fwd.target_port
    );

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        let h = handle.clone();
                        let rx = rx_bytes.clone();
                        let tx = tx_bytes.clone();
                        let th = fwd.target_host.clone();
                        let tp = fwd.target_port;
                        tokio::spawn(async move {
                            if let Err(e) = forward_connection(h, &th, tp, stream, rx, tx).await {
                                log::error!("Forward error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        log::error!("Accept error on port {}: {}", fwd.local_port, e);
                    }
                }
            }
            _ = wait_until(stopped.clone()) => {
                log::info!("Forward on port {} stopping (user)", fwd.local_port);
                break;
            }
            _ = wait_until(cancel.clone()) => {
                log::info!("Forward on port {} stopping (session reset)", fwd.local_port);
                break;
            }
        }
    }
}

async fn wait_until(flag: Arc<AtomicBool>) {
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
