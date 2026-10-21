use crate::{Backend, dispatch_http};
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde_json::{Value, json};
use std::sync::Weak;

const ALLOWED: &[&str] = &[
    "fetch_theme_path",
    "get_last_load_errors",
    "get_themes",
    "reset",
    "enable_server",
    "get_server_state",
    "get_watch_state",
    "get_backend_version",
    "dummy_function",
    "fetch_class_mappings",
    "is_standalone",
    "set_theme_state",
    "set_patch_of_theme",
    "set_component_of_theme_patch",
    "download_theme_from_url",
    "delete_theme",
    "store_read",
    "store_write",
    "toggle_watch_state",
    "generate_preset_theme",
    "generate_preset_theme_from_theme_names",
];

pub fn start(backend: Weak<Backend>) {
    tokio::spawn(async move {
        let app = Router::new()
            .route("/req", post(handle))
            .with_state(backend);
        match tokio::net::TcpListener::bind("127.0.0.1:35821").await {
            Ok(listener) => {
                log::info!("Started CSS_Loader server on port 35821");
                if let Err(error) = axum::serve(listener, app).await {
                    log::error!("CSS_Loader server failed: {error}");
                }
            }
            Err(error) => log::error!("CSS_Loader server failed to bind: {error}"),
        }
    });
}

async fn handle(
    State(backend): State<Weak<Backend>>,
    payload: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, Json<Value>) {
    let value = match payload {
        Ok(Json(value)) => value,
        Err(error) => {
            return (
                StatusCode::OK,
                Json(json!({"res": error.body_text(), "success": false})),
            );
        }
    };
    let response = async {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing method".to_owned())?;
        if !ALLOWED.contains(&method) {
            return Err(format!("Plugin has no method {method}"));
        }
        let kwargs = value
            .get("args")
            .and_then(Value::as_object)
            .ok_or_else(|| "args must be an object".to_owned())?;
        let backend = backend
            .upgrade()
            .ok_or_else(|| "Plugin has stopped".to_owned())?;
        backend.wait_ready().await;
        dispatch_http(&backend, method, kwargs).await
    }
    .await;
    let body = match response {
        Ok(res) => json!({"res": res, "success": true}),
        Err(error) => json!({"res": error, "success": false}),
    };
    (StatusCode::OK, Json(body))
}
