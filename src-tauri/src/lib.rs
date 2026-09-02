mod config;
mod tunnel;

use config::*;
use tunnel::TunnelManager;

use std::sync::Arc;
use std::time::Duration;
use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder,
};
use tokio::sync::Mutex;

// ─── Tray icon: mini logo (三层环 + 光核),整体配色随状态切换 ────────
// 与 src-tauri/icons/logo.svg 同一母题:圆角底上一组收拢的透视环,
// 中央是亮芯。状态语义沿用旧色点规则:未连接灰蓝 / 连接中黄 /
// 已连接绿 / 错误红。

struct Palette {
    top: [f32; 3],
    bot: [f32; 3],
    core: [f32; 3],
    core_b: [f32; 3],
    glow: [f32; 3],
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn palette_for(status: &TunnelStatus) -> Palette {
    match status {
        TunnelStatus::Connected => Palette {
            top: [118.0, 255.0, 192.0],
            bot: [0.0, 133.0, 88.0],
            core: [0.0, 220.0, 140.0],
            core_b: [236.0, 255.0, 247.0],
            glow: [0.0, 230.0, 150.0],
        },
        TunnelStatus::Connecting => Palette {
            top: [255.0, 222.0, 138.0],
            bot: [190.0, 124.0, 18.0],
            core: [255.0, 205.0, 96.0],
            core_b: [255.0, 246.0, 220.0],
            glow: [255.0, 205.0, 90.0],
        },
        TunnelStatus::Disconnected => Palette {
            top: [150.0, 153.0, 161.0],
            bot: [58.0, 61.0, 70.0],
            core: [118.0, 128.0, 150.0],
            core_b: [224.0, 227.0, 233.0],
            glow: [126.0, 136.0, 158.0],
        },
        TunnelStatus::Error(_) => Palette {
            top: [255.0, 128.0, 126.0],
            bot: [172.0, 28.0, 30.0],
            core: [255.0, 92.0, 90.0],
            core_b: [255.0, 228.0, 224.0],
            glow: [255.0, 85.0, 85.0],
        },
    }
}

/// 椭圆归一化距离(边界 = 1)。
fn ellipse_u(x: f32, y: f32, cx: f32, cy: f32, rx: f32, ry: f32) -> f32 {
    let dx = (x - cx) / rx;
    let dy = (y - cy) / ry;
    (dx * dx + dy * dy).sqrt()
}

/// 环带覆盖度:hw 为半宽(px),外侧 1px 抗锯齿过渡。
fn ring_cov(x: f32, y: f32, cx: f32, cy: f32, rx: f32, ry: f32, hw: f32) -> f32 {
    let u = ellipse_u(x, y, cx, cy, rx, ry);
    let pd = (u - 1.0).abs() * rx.min(ry);
    (hw + 0.5 - pd).clamp(0.0, 1.0)
}

/// 垂直渐变系数:环顶 0 → 环底 1。
fn grad_t(y: f32, cy: f32, ry: f32) -> f32 {
    ((y - (cy - ry)) / (2.0 * ry)).clamp(0.0, 1.0)
}

/// 透明合成(straight alpha)。
fn compose(acc: &mut [f32; 4], col: [f32; 3], ca: f32) {
    if ca <= 0.0 {
        return;
    }
    let na = ca + acc[3] * (1.0 - ca);
    if na <= 0.0 {
        return;
    }
    for i in 0..3 {
        acc[i] = (col[i] * ca + acc[i] * acc[3] * (1.0 - ca)) / na;
    }
    acc[3] = na;
}

fn make_tray_icon_pixels(status: &TunnelStatus) -> Vec<u8> {
    const SZ: f32 = 24.0;
    const N: usize = SZ as usize;
    let pal = palette_for(status);
    let (cx, cy) = (SZ / 2.0, SZ / 2.0); // 24px 像素中心中点 = 12.0
    let mut pixels = Vec::with_capacity(N * N * 4);

    for py in 0..N {
        for px in 0..N {
            let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
            let mut acc = [0.0f32; 4];

            // 光晕(最底层,弥散)
            let gd = ellipse_u(x, y, cx, cy, 8.1, 5.7);
            let ca = (0.45 * (1.0 - gd / 1.45)).clamp(0.0, 1.0);
            compose(&mut acc, pal.glow, ca);

            // 外环 A(最外,较宽)——母题整体放大 ~10%,贴满画布:
            // 托盘里其他程序图标普遍画到边缘,留白会显得小一圈
            let ca = ring_cov(x, y, cx, cy, 11.6, 9.7, 1.45);
            compose(&mut acc, lerp3(pal.top, pal.bot, grad_t(y, cy, 9.7)), ca);
            // 环 B
            let ca = ring_cov(x, y, cx, cy, 8.7, 7.3, 1.15);
            compose(&mut acc, lerp3(pal.top, pal.bot, grad_t(y, cy, 7.3)), ca);
            // 环 C(最内)
            let ca = ring_cov(x, y, cx, cy, 5.7, 4.8, 0.95);
            compose(&mut acc, lerp3(pal.top, pal.bot, grad_t(y, cy, 4.8)), ca);

            // 光核:内芯白热(gd < 0.45 保持白),向外过渡到主题色
            let gd = ellipse_u(x, y, cx, cy, 4.3, 2.3);
            if gd <= 1.2 {
                let ca = ((1.2 - gd) / 0.25).clamp(0.0, 1.0);
                let tint = ((gd - 0.45) / 0.55).clamp(0.0, 1.0);
                let col = lerp3(pal.core_b, pal.core, tint);
                compose(&mut acc, col, ca);
            }

            // 通道保持 0..255 线性尺度(与调色板一致),alpha 为 0..1
            pixels.push(acc[0].clamp(0.0, 255.0) as u8);
            pixels.push(acc[1].clamp(0.0, 255.0) as u8);
            pixels.push(acc[2].clamp(0.0, 255.0) as u8);
            pixels.push((acc[3] * 255.0).clamp(0.0, 255.0) as u8);
        }
    }
    pixels
}

/// Leak the pixel data to get a `&'static [u8]` so we can return `Image<'static>`.
fn tray_icon(status: &TunnelStatus) -> Image<'static> {
    let data = make_tray_icon_pixels(status);
    let leaked: &'static [u8] = Box::leak(data.into_boxed_slice());
    Image::new(leaked, 24, 24)
}

/// 托盘菜单行前缀(● 已连 / ◐ 连接中 / ○ 未连 / ● 错误)。
fn status_marker(status: &TunnelStatus) -> &'static str {
    match status {
        TunnelStatus::Connected => "●",
        TunnelStatus::Connecting => "◐",
        TunnelStatus::Error(_) => "●",
        TunnelStatus::Disconnected => "○",
    }
}

fn status_text(status: &TunnelStatus) -> &'static str {
    match status {
        TunnelStatus::Connected => "已连接",
        TunnelStatus::Connecting => "连接中",
        TunnelStatus::Error(_) => "错误",
        TunnelStatus::Disconnected => "未连接",
    }
}

/// 托盘集中控制入口:汇总图标颜色 + 每隧道一行的动态菜单。
///
/// 触发时机:任何状态转换(publish/remove_runtime)与配置保存——**不**在
/// 1Hz 指标 tick 时触发。菜单事件 handler 注册在 app 全局监听列表,
/// set_menu 换菜单后事件仍能路由,因此每次重建整份菜单即可。
///
/// 锁纪律:先取 config 锁、后取 running 锁,克隆数据后立即放锁,
/// 绝不在持锁状态下做任何 GUI 调用。
pub async fn refresh_tray(app: &tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let tunnels = { state.config.lock().await.tunnels.clone() };
    let active = state.manager.active_statuses().await;

    // ── 汇总状态:任一错误→红;任一连接中→黄;全部已连→绿;否则灰蓝 ──
    let mut any_error = false;
    let mut any_connecting = false;
    let mut all_connected = true;
    for t in &tunnels {
        match active.get(&t.id) {
            Some(TunnelStatus::Error(_)) => any_error = true,
            Some(TunnelStatus::Connecting) => any_connecting = true,
            Some(TunnelStatus::Connected) => {}
            _ => all_connected = false,
        }
    }
    let aggregate = if any_error {
        TunnelStatus::Error(String::new())
    } else if any_connecting {
        TunnelStatus::Connecting
    } else if !tunnels.is_empty() && all_connected {
        TunnelStatus::Connected
    } else {
        TunnelStatus::Disconnected
    };

    // ── 重建菜单 ─────────────────────────────────────────────────────
    let mut tunnel_items = Vec::new();
    for t in &tunnels {
        let status = active
            .get(&t.id)
            .cloned()
            .unwrap_or(TunnelStatus::Disconnected);
        tunnel_items.push(
            MenuItemBuilder::with_id(
                format!("tunnel_{}", t.id),
                format!(
                    "{} {}  {}",
                    status_marker(&status),
                    t.name,
                    status_text(&status)
                ),
            )
            .build(app)
            .map_err(|e| e.to_string())?,
        );
    }
    let all_start = MenuItemBuilder::with_id("all_start", "全部连接")
        .build(app)
        .map_err(|e| e.to_string())?;
    let all_stop = MenuItemBuilder::with_id("all_stop", "全部断开")
        .build(app)
        .map_err(|e| e.to_string())?;
    let config_item = MenuItemBuilder::with_id("open_config", "配置")
        .build(app)
        .map_err(|e| e.to_string())?;
    let quit_item = MenuItemBuilder::with_id("quit", "退出")
        .build(app)
        .map_err(|e| e.to_string())?;

    let mut builder = MenuBuilder::new(app);
    for item in &tunnel_items {
        builder = builder.item(item);
    }
    if !tunnel_items.is_empty() {
        builder = builder.separator();
    }
    let menu = builder
        .item(&all_start)
        .item(&all_stop)
        .separator()
        .item(&config_item)
        .item(&quit_item)
        .build()
        .map_err(|e| e.to_string())?;

    if let Some(tray) = app.tray_by_id("tunnel_tray") {
        let _ = tray.set_icon(Some(tray_icon(&aggregate)));
        let _ = tray.set_menu(Some(menu));
    }
    Ok(())
}

#[tauri::command]
async fn get_config(state: tauri::State<'_, AppState>) -> Result<AppConfig, String> {
    Ok(state.config.lock().await.clone())
}

/// 新打开的窗口做状态初始化:事件是瞬时的,只靠 listen 会漏掉
/// Connecting/Error 等转换瞬间。
#[tauri::command]
async fn get_tunnel_statuses(state: tauri::State<'_, AppState>) -> Result<Vec<TunnelMetric>, String> {
    Ok(state.manager.status_metrics().await)
}

/// 整体替换隧道列表(多主机集中管理)。
/// - 为缺 id 的新条目分配 id 并返回更新后的配置(前端据此同步);
/// - 已不在列表中的运行中隧道会被自动停止(删除安全);
/// - 通知所有窗口(config-updated)并刷新托盘菜单。
#[tauri::command]
async fn save_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    tunnels: Vec<TunnelConfig>,
) -> Result<AppConfig, String> {
    let mut new_cfg = AppConfig {
        tunnels,
        tunnel: None,
    };
    new_cfg.ensure_ids();

    {
        let mut cfg = state.config.lock().await;
        *cfg = new_cfg.clone();
    } // 锁纪律:config 锁先释放,再动 running 锁

    let new_ids: Vec<String> = new_cfg.tunnels.iter().map(|t| t.id.clone()).collect();
    state.manager.prune(&new_ids).await;
    state.manager.refresh_names(&new_cfg.tunnels).await;

    save_config_to_file(&state.config_path, &new_cfg);
    let _ = app.emit("config-updated", ());
    refresh_tray(&app).await?;
    Ok(new_cfg)
}

#[tauri::command]
async fn show_config_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("config_panel") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }

    let _window =
        WebviewWindowBuilder::new(&app, "config_panel", WebviewUrl::App("index.html".into()))
            .title("Tunnel - 配置")
            .inner_size(480.0, 640.0)
            .maximizable(false)
            .center()
            .build()
            .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
async fn start_tunnel(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    state.manager.start_tunnel(&id).await
}

#[tauri::command]
async fn stop_tunnel(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    state.manager.stop_tunnel(&id).await;
    Ok(())
}

struct AppState {
    config: Arc<Mutex<AppConfig>>,
    config_path: std::path::PathBuf,
    manager: Arc<TunnelManager>,
}

fn load_config(path: &std::path::Path) -> AppConfig {
    match std::fs::read_to_string(path) {
        Ok(json) => {
            let mut cfg: AppConfig = serde_json::from_str(&json).unwrap_or_else(|e| {
                log::warn!("Failed to parse config, using defaults: {}", e);
                AppConfig::defaults()
            });
            // 旧单隧道文件迁移进列表 + 补全新条目的 id
            cfg.migrate_and_ensure_ids();
            cfg.decrypt_passwords();
            cfg
        }
        Err(_) => {
            log::info!("No saved config found, using defaults");
            AppConfig::defaults()
        }
    }
}

fn save_config_to_file(path: &std::path::Path, config: &AppConfig) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut to_save = config.clone();
    to_save.encrypt_passwords();
    match serde_json::to_string_pretty(&to_save) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                log::error!("Failed to save config: {}", e);
            }
        }
        Err(e) => log::error!("Failed to serialize config: {}", e),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(win) = app.get_webview_window("mini_widget") {
                let _ = win.unminimize();
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // ── System Tray ────────────────────────────────────────────
            // 菜单内容动态(每隧道一项),由 refresh_tray 在状态转换/配置
            // 保存后整份重建;on_menu_event 注册在全局监听列表,换菜单后
            // 事件依然路由到这里,因此这里不再维护静态菜单项。
            let _tray = TrayIconBuilder::with_id("tunnel_tray")
                .icon(tray_icon(&TunnelStatus::Disconnected))
                .show_menu_on_left_click(false)
                .tooltip("Tunnel")
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "all_start" => {
                        let mgr = app.state::<AppState>().manager.clone();
                        tauri::async_runtime::spawn(async move {
                            mgr.start_all().await;
                        });
                    }
                    "all_stop" => {
                        let mgr = app.state::<AppState>().manager.clone();
                        tauri::async_runtime::spawn(async move {
                            mgr.stop_all().await;
                        });
                    }
                    id => {
                        if let Some(tunnel_id) = id.strip_prefix("tunnel_") {
                            // 单隧道行:点击切换启/停
                            let mgr = app.state::<AppState>().manager.clone();
                            let tunnel_id = tunnel_id.to_string();
                            tauri::async_runtime::spawn(async move {
                                if mgr.is_running(&tunnel_id).await {
                                    mgr.stop_tunnel(&tunnel_id).await;
                                } else if let Err(e) = mgr.start_tunnel(&tunnel_id).await {
                                    log::warn!("托盘启动隧道失败: {}", e);
                                }
                            });
                        } else {
                            match id {
                                "open_config" => {
                                    let handle = app.clone();
                                    tauri::async_runtime::spawn(async move {
                                        let _ = show_config_window(handle).await;
                                    });
                                }
                                "quit" => {
                                    log::info!("Quitting Tunnel...");
                                    // 先协作式停掉所有隧道(SSH 通道礼貌关闭),
                                    // 再销毁窗口让 WebView2 顺畅释放资源
                                    let mgr = app.state::<AppState>().manager.clone();
                                    let handle = app.clone();
                                    tauri::async_runtime::spawn(async move {
                                        mgr.stop_all().await;
                                        tokio::time::sleep(Duration::from_millis(300)).await;
                                        for window in handle.webview_windows().values() {
                                            let _ = window.destroy();
                                        }
                                        handle.exit(0);
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(win) = app.get_webview_window("mini_widget") {
                            let _ = win.unminimize();
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                })
                .build(app)?;

            // ── Mini Widget Window ─────────────────────────────────────
            let mini = app.get_webview_window("mini_widget").unwrap();
            let mini_clone = mini.clone();
            mini.on_window_event(move |event| {
                match event {
                    // 1. 拦截关闭信号，改为隐藏
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        let _ = mini_clone.hide();
                    }
                    // 2. 捕获多屏缩放率改变
                    tauri::WindowEvent::ScaleFactorChanged { .. } => {
                        let w = mini_clone.clone();
                        tauri::async_runtime::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            if let Ok(size) = w.inner_size() {
                                let _ = w.set_size(size);
                                #[cfg(target_os = "windows")]
                                {
                                    let _ = w.set_decorations(false);
                                }
                            }
                        });
                    }
                    _ => {}
                }
            });

            // ── Application State ──────────────────────────────────────
            let app_handle = app.handle().clone();
            let config_dir = app
                .path()
                .app_config_dir()
                .expect("failed to resolve config dir");
            let config_path = config_dir.join("tunnel.json");
            let config = load_config(&config_path);
            let config_arc = Arc::new(Mutex::new(config));

            let manager = Arc::new(TunnelManager::new(app_handle.clone(), config_arc.clone()));

            let state = AppState {
                config: config_arc,
                config_path,
                manager,
            };
            app.manage(state);

            // 启动时按已加载配置刷一次托盘(全部未连接);setup 是同步
            // 闭包,刷新任务放到 async runtime 上执行。
            let handle = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = refresh_tray(&handle).await {
                    log::warn!("初始化托盘失败: {}", e);
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_tunnel_statuses,
            save_config,
            show_config_window,
            start_tunnel,
            stop_tunnel,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(pixels: &[u8], x: usize, y: usize) -> (u8, u8, u8, u8) {
        let i = (y * 24 + x) * 4;
        (pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3])
    }

    /// 托盘徽标母题自检:角落透明 / 中心白热光核 / 环带着色 /
    /// 四种状态的配色区分(回归保护,改动图标渲染时运行)。
    #[test]
    fn tray_icon_motif_and_palette() {
        // 角落透明
        let px = make_tray_icon_pixels(&TunnelStatus::Connected);
        assert_eq!(pixel(&px, 0, 0).3, 0, "corner must be transparent");
        assert_eq!(pixel(&px, 23, 23).3, 0, "corner must be transparent");

        // 中心光核:不透明的白热绿
        let (r, g, b, a) = pixel(&px, 12, 12);
        assert_eq!(a, 255, "core must be opaque");
        assert!(r > 200 && g > 230 && b > 230, "core should be near-white, got ({r},{g},{b})");

        // 外环顶部:环带存在且为绿色系(绿分量显著高于红)
        let (r, g, _, a) = pixel(&px, 12, 2);
        assert!(a > 200, "ring band must be opaque");
        assert!(g as i32 - r as i32 > 30, "connected ring should be greenish, got ({r},{g})");

        // 未连接 → 中性灰(绿红接近),与已连接区分
        let pxg = make_tray_icon_pixels(&TunnelStatus::Disconnected);
        let (r2, g2, _, a2) = pixel(&pxg, 12, 2);
        assert!(a2 > 200);
        assert!(
            (g2 as i32 - r2 as i32).abs() < 15,
            "disconnected ring should be neutral gray, got ({r2},{g2})"
        );

        // 错误 → 红系(红分量显著高于绿)
        let pxe = make_tray_icon_pixels(&TunnelStatus::Error("test".into()));
        let (r3, g3, _, _) = pixel(&pxe, 12, 2);
        assert!(r3 as i32 - g3 as i32 > 30, "error ring should be reddish, got ({r3},{g3})");
    }
}
