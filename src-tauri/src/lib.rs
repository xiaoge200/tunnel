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

#[tauri::command]
async fn get_tunnels(state: tauri::State<'_, AppState>) -> Result<Vec<TunnelConfig>, String> {
    Ok(state.config.lock().await.tunnels.clone())
}

#[tauri::command]
async fn save_tunnels(
    state: tauri::State<'_, AppState>,
    tunnels: Vec<TunnelConfig>,
) -> Result<(), String> {
    state.config.lock().await.tunnels = tunnels;
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
async fn start_tunnel(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    config: TunnelConfig,
) -> Result<(), String> {
    let mgr = state.manager.clone();
    mgr.start_tunnel(config).await;
    if let Some(tray) = app.tray_by_id("tunnel_tray") {
        let _ = tray.set_icon(Some(tray_icon(255, 200, 0)));
    }
    Ok(())
}

#[tauri::command]
async fn stop_tunnel(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let mgr = state.manager.clone();
    mgr.stop_tunnel(&id).await;
    if let Some(tray) = app.tray_by_id("tunnel_tray") {
        let _ = tray.set_icon(Some(tray_icon(120, 120, 120)));
    }
    Ok(())
}

struct AppState {
    config: Arc<Mutex<AppConfig>>,
    manager: Arc<TunnelManager>,
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
                        log::info!("Disconnect all requested");
                        let mgr = app.state::<AppState>().manager.clone();
                        tauri::async_runtime::spawn(async move {
                            mgr.stop_all_tunnels().await;
                        });
                        if let Some(tray) = app.tray_by_id("tunnel_tray") {
                            let _ = tray.set_icon(Some(tray_icon(120, 120, 120)));
                        }
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
            let manager = Arc::new(TunnelManager::new(app_handle.clone()));

            let state = AppState {
                config: Arc::new(Mutex::new(AppConfig::defaults())),
                manager,
            };
            app.manage(state);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_tunnels,
            save_tunnels,
            show_config_window,
            start_tunnel,
            stop_tunnel,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
