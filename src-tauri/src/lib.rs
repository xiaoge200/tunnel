mod config;
mod tunnel;

use config::*;
use tunnel::TunnelManager;

use std::sync::Arc;
use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, WebviewUrl, WebviewWindowBuilder,
};
use tokio::sync::Mutex;

fn make_tray_icon_pixels(r: u8, g: u8, b: u8) -> Vec<u8> {
    let size = 24u32;
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    let cx = size as f32 / 2.0;
    let cy = size as f32 / 2.0;
    let radius = size as f32 / 2.0 - 3.0;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= radius {
                let alpha = if dist > radius - 1.5 {
                    ((radius - dist).max(0.0).min(1.0) * 255.0) as u8
                } else {
                    255
                };
                pixels.push(r);
                pixels.push(g);
                pixels.push(b);
                pixels.push(alpha);
            } else {
                pixels.push(0);
                pixels.push(0);
                pixels.push(0);
                pixels.push(0);
            }
        }
    }
    pixels
}

/// Leak the pixel data to get a `&'static [u8]` so we can return `Image<'static>`.
fn tray_icon(r: u8, g: u8, b: u8) -> Image<'static> {
    let data = make_tray_icon_pixels(r, g, b);
    let leaked: &'static [u8] = Box::leak(data.into_boxed_slice());
    Image::new(leaked, 24, 24)
}

pub fn set_tray_icon(app: &tauri::AppHandle, status: TunnelStatus) {
    if let Some(tray) = app.tray_by_id("tunnel_tray") {
        let icon_image = match status {
            TunnelStatus::Connected => tray_icon(0, 255, 0), // 绿色
            TunnelStatus::Connecting => tray_icon(255, 200, 0), // 亮黄色
            TunnelStatus::Disconnected => tray_icon(120, 120, 120), // 灰色
            TunnelStatus::Error(_) => tray_icon(255, 0, 0),  // 红色
        };
        let _ = tray.set_icon(Some(icon_image));
    }
}

#[tauri::command]
async fn get_config(state: tauri::State<'_, AppState>) -> Result<AppConfig, String> {
    Ok(state.config.lock().await.clone())
}

#[tauri::command]
async fn save_config(
    state: tauri::State<'_, AppState>,
    config: TunnelConfig,
) -> Result<(), String> {
    let mut app_config = state.config.lock().await;
    app_config.tunnel = Some(config);
    save_config_to_file(&state.config_path, &app_config);
    Ok(())
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
            .inner_size(360.0, 520.0)
            .maximizable(false)
            .resizable(false)
            .center()
            .build()
            .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
async fn start_tunnel(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mgr = state.manager.clone();
    mgr.start_tunnel().await;
    Ok(())
}

#[tauri::command]
async fn stop_tunnel(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mgr = state.manager.clone();
    mgr.stop_tunnel().await;
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
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // ── System Tray ────────────────────────────────────────────
            let disconnect_item = MenuItemBuilder::with_id("connect", "连接/断开").build(app)?;
            let config_item = MenuItemBuilder::with_id("open_config", "配置").build(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "退出").build(app)?;

            let menu = MenuBuilder::new(app)
                .item(&disconnect_item)
                .item(&config_item)
                .separator()
                .item(&quit_item)
                .build()?;

            let _tray = TrayIconBuilder::with_id("tunnel_tray")
                .icon(tray_icon(120, 120, 120))
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("Tunnel")
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "connect" => {
                        let mgr = app.state::<AppState>().manager.clone();
                        tauri::async_runtime::spawn(async move {
                            if mgr.is_running().await {
                                mgr.stop_tunnel().await;
                            } else {
                                mgr.start_tunnel().await;
                            }
                        });
                    }
                    "open_config" => {
                        let handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = show_config_window(handle).await;
                        });
                    }
                    "quit" => {
                        log::info!("Quitting Tunnel...");
                        // 1. 先把所有活着的网页窗口强制关闭，让 WebView2 顺畅地释放资源
                        for window in app.webview_windows().values() {
                            let _ = window.destroy();
                        }
                        app.exit(0);
                    }
                    _ => {}
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
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = mini_clone.hide();
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

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config,
            show_config_window,
            start_tunnel,
            stop_tunnel,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
