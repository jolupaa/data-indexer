use anyhow::Result;

use axum::{
    routing::{delete, get, post},
    Router,
};

use std::sync::{Arc, Mutex};

use tantivy::{
    schema::{Schema, STRING, STORED, TEXT},
    Index,
};

use crate::indexer::{
    AppState,
    delete_document,
    upsert_document,
};

use crate::searcher::search_documents;
use crate::init::{SearchFields};

pub async fn start_server() -> Result<()> {

    let mut schema_builder = Schema::builder();

    let fields = SearchFields {
        id: schema_builder.add_text_field("id", STRING | STORED),
        uid: schema_builder.add_text_field("uid", STRING | STORED),
        tipo: schema_builder.add_text_field("tipo", STRING | STORED),
        info_title: schema_builder.add_text_field("info_title", TEXT | STORED),
        titulo: schema_builder.add_text_field("titulo", TEXT | STORED),
        subtitulo: schema_builder.add_text_field("subtitulo", TEXT | STORED),
        contenido: schema_builder.add_text_field("contenido", TEXT),
        fecha: schema_builder.add_text_field("fecha", STRING | STORED),
    };

    let _schema = schema_builder.build();

    let index = Index::open_in_dir("./search_index")?;

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