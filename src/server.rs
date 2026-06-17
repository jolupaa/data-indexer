use anyhow::Result;

use axum::{
    routing::{delete, get, post},
    Router,
};

use std::sync::{Arc, Mutex};

use tantivy::Index;

use crate::indexer::{
    AppState,
    delete_document,
    upsert_document,
};

use crate::searcher::search_documents;
use crate::init::{build_schema, register_tokenizers};

pub async fn start_server() -> Result<()> {

    // El esquema en disco es el que manda; sólo necesitamos los `Field` handles,
    // que `build_schema` reconstruye en el mismo orden que usó `reindex`.
    let (_schema, fields) = build_schema();

    let index = Index::open_in_dir("./search_index")?;
    register_tokenizers(&index);

    let index_writer = index.writer(50_000_000)?;

    let state = AppState {
        index,
        writer: Arc::new(Mutex::new(index_writer)),
        fields,
    };

    let app = Router::new()
        .route("/index/upsert", post(upsert_document))
        .route("/index/delete", delete(delete_document))
        .route("/search", get(search_documents))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:5000").await?;

    axum::serve(listener, app).await?;

    Ok(())
}