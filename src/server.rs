use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    routing::{delete, get, post},
};
use serde::Serialize;

use crate::indexer::{
    delete_document, delete_thread, delete_tipo, upsert_document, upsert_documents,
};
use crate::init::{SCHEMA_VERSION, explain_lock_error, open_index};
use crate::searcher::search_documents;
use crate::state::AppState;
use crate::stats::stats;
use crate::utils::*;

/// Tamaño máximo del cuerpo de los upserts. El de axum por defecto (2 MB), que
/// se mantiene en el resto de rutas, se queda corto para el texto extraído de
/// documentos largos, que `reindex` sí indexa sin límite.
pub const MAX_UPSERT_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Lo que sabe hacer esta versión. El backend lo consulta antes de mandar
/// documentos de chat: un indexer sin `acl` los dejaría públicos.
pub const FEATURES: &[&str] = &[
    "acl",
    "thread",
    "stats",
    "prefix",
    "delete_thread",
    "delete_tipo",
];

pub fn router(state: AppState) -> Router {
    let upsert_limit = DefaultBodyLimit::max(MAX_UPSERT_BODY_BYTES);
    Router::new()
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/index/upsert", post(upsert_document).layer(upsert_limit))
        .route(
            "/index/upsert/batch",
            post(upsert_documents).layer(upsert_limit),
        )
        .route("/index/delete", delete(delete_document))
        .route("/index/delete/thread", delete(delete_thread))
        .route("/index/delete/tipo", delete(delete_tipo))
        .route("/search", get(search_documents))
        .with_state(state)
}

pub async fn start_server() -> Result<()> {
    let dir = index_dir();
    let (index, fields) = open_index(&dir)?;
    let state = AppState::new(index, fields).map_err(|err| explain_lock_error(err, &dir))?;

    let addr = bind_addr();
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("no se pudo escuchar en {addr}"))?;
    println!(
        "Sirviendo {} en http://{}",
        dir.display(),
        listener.local_addr()?
    );

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    println!("Servidor detenido.");
    Ok(())
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub docs: u64,
    pub schema: u32,
    pub features: &'static [&'static str],
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        docs: state.reader.searcher().num_docs(),
        schema: SCHEMA_VERSION,
        features: FEATURES,
    })
}

/// Espera a Ctrl+C o SIGTERM para cerrar ordenadamente: se terminan las
/// peticiones en curso (y sus commits) antes de salir.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
