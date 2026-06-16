use axum::{
    extract::State,
    Json,
};

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tantivy::{
    doc,
    IndexWriter,
    Term,
    Index
};

use crate::init::SearchFields;

use crate::lib::*;

#[derive(Clone)]
pub struct AppState {
    pub index: Index,
    pub writer: Arc<Mutex<IndexWriter>>,
    pub fields: SearchFields,
}

#[derive(Deserialize)]
pub struct IndexDocumentRequest {
    pub id: String,
    pub tipo: String,
    pub titulo: String,
    pub subtitulo: String,
    pub contenido: String,
    pub fecha: String
}

#[derive(Serialize)]
pub struct ApiResponse {
    pub ok: bool,
}

pub async fn upsert_document(
    State(state): State<AppState>,
    Json(payload): Json<IndexDocumentRequest>,
) -> Json<ApiResponse> {
    let mut writer = state.writer.lock().unwrap();

    let uid = make_uid(&payload.tipo, &payload.id);

    let term = Term::from_field_text(state.fields.uid, &uid);
    writer.delete_term(term);

    writer.add_document(doc!(
        state.fields.uid => uid,
        state.fields.id => payload.id,
        state.fields.tipo => payload.tipo,
        state.fields.titulo => payload.titulo,
        state.fields.subtitulo => payload.subtitulo,
        state.fields.contenido => payload.contenido,
        state.fields.fecha => payload.fecha,
    )).unwrap();

    writer.commit().unwrap();

    Json(ApiResponse { ok: true })
}

#[derive(Deserialize)]
pub struct DeleteDocumentRequest {
    pub id: String,
    pub tipo: String,
}

pub async fn delete_document(
    State(state): State<AppState>,
    Json(payload): Json<DeleteDocumentRequest>,
) -> Json<ApiResponse> {
    let mut writer = state.writer.lock().unwrap();

    let uid = make_uid(&payload.tipo, &payload.id);

    let term = Term::from_field_text(state.fields.uid, &uid);

    writer.delete_term(term);

    writer.commit().unwrap();

    Json(ApiResponse { ok: true })
}