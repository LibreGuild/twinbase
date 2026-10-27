//! TwinBase desktop shell: hosts the core, forwards its events to the UI, exposes one `api`
//! command, and serves MCP on 127.0.0.1 while the window is open.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
mod traffic_lights;

use std::sync::Arc;

use serde_json::Value;
use tauri::{Emitter, Manager, RunEvent, State, WindowEvent};
use tokio::sync::broadcast::error::RecvError;
use twinbase_core::{Actor, App, CoreError};

#[tauri::command]
async fn api(app: State<'_, Arc<App>>, method: String, params: Value) -> Result<Value, CoreError> {
    twinbase_core::api::call(&app, &method, params, &Actor::User).await
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();

    let core = match App::new(twinbase_core::settings::app_dir(), None) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("TwinBase could not open its workspace: {}", e.message);
            std::process::exit(1);
        }
    };

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(core.clone())
        .invoke_handler(tauri::generate_handler![api])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            for window in app.webview_windows().values() {
                traffic_lights::center(&window.as_ref().window());
            }
            let handle = app.handle().clone();
            let core = app.state::<Arc<App>>().inner().clone();
            tauri::async_runtime::spawn(async move {
                // Forward core events to the UI.
                let mut rx = core.subscribe();
                let forward = handle.clone();
                tauri::async_runtime::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(event) => {
                                let _ = forward.emit("twinbase://event", &event);
                            }
                            Err(RecvError::Lagged(_)) => continue,
                            Err(RecvError::Closed) => break,
                        }
                    }
                });
                core.autostart_mocks().await;
                let port = core.settings().mcp.port;
                match twinbase_core::http::serve(core.clone(), twinbase_core::http::HttpOptions { port, dev_api: false }).await {
                    Ok((addr, _task)) => tracing::info!("MCP endpoint on http://{addr}/mcp"),
                    Err(e) => tracing::warn!("MCP endpoint unavailable on port {port}: {e} (is another TwinBase running?)"),
                }
            });
            Ok(())
        })
        .on_window_event(|_window, _event| {
            #[cfg(target_os = "macos")]
            if matches!(
                _event,
                WindowEvent::Resized(_) | WindowEvent::Focused(_) | WindowEvent::ThemeChanged(_) | WindowEvent::ScaleFactorChanged { .. }
            ) {
                traffic_lights::center(_window);
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build the TwinBase window");

    app.run(move |handle, event| {
        if let RunEvent::Exit = event {
            // Stop mock servers cleanly so their data is flushed to the workspace.
            let core = handle.state::<Arc<App>>().inner().clone();
            tauri::async_runtime::block_on(core.stop_all_mocks());
        }
    });
}
