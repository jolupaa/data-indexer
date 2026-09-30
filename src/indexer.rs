use axum::{Json, extract::State};
use serde::{Deserialize, Deserializer, Serialize};
use tantivy::{TantivyDocument, Term, doc};

use crate::error::ApiError;
use crate::init::SearchFields;
use crate::state::AppState;
use crate::utils::*;

#[derive(Deserialize)]
pub struct IndexDocumentRequest {
    pub id: String,
    pub tipo: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub titulo: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub subtitulo: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub contenido: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub fecha: String,
    /// Sólo lo usan los `info_doc`. Sin él, actualizar un `info_doc` por la API
    /// le borraría el `info_title` que le puso `reindex`.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub info_title: String,
}

/// Acepta `null` en los campos de texto opcionales y lo trata como "", igual
/// que hace `reindex` con los NULL de la base de datos.
fn null_as_empty<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Serialize)]
pub struct ApiResponse {
    pub ok: bool,
}

#[derive(Serialize)]
pub struct BatchResponse {
    pub ok: bool,
    pub indexed: usize,
}

/// Longitud máxima de `tipo` e `id`. tantivy descarta en silencio los términos
/// de más de 64 KB, y un `uid` descartado haría el documento imposible de
/// borrar o reemplazar (cada upsert lo duplicaría).
const MAX_KEY_BYTES: usize = 1024;

/// Valida el par (`tipo`, `id`) y devuelve su `uid`.
pub fn document_uid(tipo: &str, id: &str) -> Result<String, ApiError> {
    if tipo.is_empty() || id.trim().is_empty() {
        return Err(ApiError::bad_request(
            "`tipo` e `id` no pueden estar vacíos",
        ));
    }
    if tipo.len() > MAX_KEY_BYTES || id.len() > MAX_KEY_BYTES {
        return Err(ApiError::bad_request(format!(
            "`tipo` e `id` no pueden superar {MAX_KEY_BYTES} bytes"
        )));
    }
    // El uid es "tipo:id". Con ':' en el tipo, dos documentos distintos podrían
    // compartirlo ("a:b" + "c" y "a" + "b:c") y un upsert borraría al otro.
    if tipo.contains(':') {
        return Err(ApiError::bad_request("`tipo` no puede contener ':'"));
    }
    Ok(make_uid(tipo, id))
}

/// Valida la petición y construye el documento junto con el término de su
/// `uid`.
pub fn build_document(
    fields: &SearchFields,
    mut payload: IndexDocumentRequest,
) -> Result<(Term, TantivyDocument), ApiError> {
    // `/search` compara el filtro `tipo` ya recortado: lo guardamos igual.
    payload.tipo = payload.tipo.trim().to_string();
    let uid = document_uid(&payload.tipo, &payload.id)?;
    Ok(into_document(fields, payload, uid))
}

/// Construye el documento de una petición ya validada. Es la única definición
/// de la forma de un documento: la usan tanto la API como `reindex`.
pub fn into_document(
    fields: &SearchFields,
    payload: IndexDocumentRequest,
    uid: String,
) -> (Term, TantivyDocument) {
    let term = Term::from_field_text(fields.uid, &uid);

    let mut document = doc!(
        fields.uid => uid,
        fields.id => payload.id,
        fields.tipo => payload.tipo,
        fields.titulo => payload.titulo,
        fields.subtitulo => payload.subtitulo,
        fields.contenido => payload.contenido,
        fields.fecha => payload.fecha,
    );
    // Sólo los documentos que lo tienen (los `info_doc`) llevan `info_title`.
    if !payload.info_title.is_empty() {
        document.add_text(fields.info_title, payload.info_title);
    }
    (term, document)
}

pub async fn upsert_document(
    State(state): State<AppState>,
    Json(payload): Json<IndexDocumentRequest>,
) -> Result<Json<ApiResponse>, ApiError> {
    let (term, document) = build_document(&state.fields, payload)?;

    state
        .write(move |writer| {
            writer.delete_term(term);
            writer.add_document(document)?;
            Ok(())
        })
        .await?;

    Ok(Json(ApiResponse { ok: true }))
}

/// Upsert de varios documentos con un único commit: mucho más barato que una
/// llamada a `/index/upsert` por documento. Es todo o nada.
pub async fn upsert_documents(
    State(state): State<AppState>,
    Json(payload): Json<Vec<IndexDocumentRequest>>,
) -> Result<Json<BatchResponse>, ApiError> {
    let documents = payload
        .into_iter()
        .enumerate()
        .map(|(index, item)| build_document(&state.fields, item).map_err(|err| err.for_item(index)))
        .collect::<Result<Vec<_>, _>>()?;
    let indexed = documents.len();

    if indexed > 0 {
        state
            .write(move |writer| {
                for (term, document) in documents {
                    writer.delete_term(term);
                    writer.add_document(document)?;
                }
                Ok(())
            })
            .await?;
    }

    Ok(Json(BatchResponse { ok: true, indexed }))
}

#[derive(Deserialize)]
pub struct DeleteDocumentRequest {
    pub id: String,
    pub tipo: String,
}

pub async fn delete_document(
    State(state): State<AppState>,
    Json(payload): Json<DeleteDocumentRequest>,
) -> Result<Json<ApiResponse>, ApiError> {
    let uid = document_uid(payload.tipo.trim(), &payload.id)?;
    let term = Term::from_field_text(state.fields.uid, &uid);

    state
        .write(move |writer| {
            writer.delete_term(term);
            Ok(())
        })
        .await?;

    Ok(Json(ApiResponse { ok: true }))
}
