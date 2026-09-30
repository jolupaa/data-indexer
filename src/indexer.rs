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

/// Valida el par (`tipo`, `id`) y devuelve el término de su `uid`.
fn uid_term(fields: &SearchFields, tipo: &str, id: &str) -> Result<Term, ApiError> {
    if tipo.is_empty() || id.trim().is_empty() {
        return Err(ApiError::bad_request(
            "`tipo` e `id` no pueden estar vacíos",
        ));
    }
    // El uid es "tipo:id". Con ':' en el tipo, dos documentos distintos podrían
    // compartirlo ("a:b" + "c" y "a" + "b:c") y un upsert borraría al otro.
    if tipo.contains(':') {
        return Err(ApiError::bad_request("`tipo` no puede contener ':'"));
    }
    Ok(Term::from_field_text(fields.uid, &make_uid(tipo, id)))
}

fn build_document(
    fields: &SearchFields,
    payload: IndexDocumentRequest,
) -> Result<(Term, TantivyDocument), ApiError> {
    // `/search` compara el filtro `tipo` ya recortado: lo guardamos igual.
    let tipo = payload.tipo.trim();
    let term = uid_term(fields, tipo, &payload.id)?;
    let uid = make_uid(tipo, &payload.id);

    let document = doc!(
        fields.uid => uid,
        fields.id => payload.id,
        fields.tipo => tipo,
        fields.titulo => payload.titulo,
        fields.subtitulo => payload.subtitulo,
        fields.contenido => payload.contenido,
        fields.fecha => payload.fecha,
        fields.info_title => payload.info_title,
    );
    Ok((term, document))
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
        .map(|item| build_document(&state.fields, item))
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
    let term = uid_term(&state.fields, payload.tipo.trim(), &payload.id)?;

    state
        .write(move |writer| {
            writer.delete_term(term);
            Ok(())
        })
        .await?;

    Ok(Json(ApiResponse { ok: true }))
}
