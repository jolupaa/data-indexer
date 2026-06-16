use anyhow::Result;

use axum::{
    extract::State,
    routing::post,
    Json,
    Router,
};

#[derive(Clone)]
struct AppState {
    writer: Arc<Mutex<IndexWriter>>,
    fields: SearchFields,
}

#[derive(Deserialize)]
struct IndexDocumentRequest {
    id: String,
    tipo: String,
    titulo: String,
    subtitulo: String,
    contenido: String,
    fecha: String
}

#[derive(Serialize)]
struct ApiResponse {
    ok: bool,
}

fn make_uid(tipo: &str, id: &str) -> String {
    format!("{}:{}", tipo, id)
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