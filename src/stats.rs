//! `GET /stats`: cuántos documentos hay en el índice y de cada `tipo`.

use axum::{Json, extract::State};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use tantivy::{
    Searcher, Term,
    collector::Count,
    query::TermQuery,
    schema::{Field, IndexRecordOption},
};

use crate::error::ApiError;
use crate::state::AppState;

/// Los corpus que mantiene el backend. Salen siempre en `by_tipo`, aunque sea
/// con 0, para que el backend distinga "no hay ninguno" de "no se informa".
pub const KNOWN_TIPOS: [&str; 3] = ["noticia", "info_doc", "chat_msg"];

#[derive(Serialize)]
pub struct StatsResponse {
    pub total: u64,
    pub by_tipo: BTreeMap<String, u64>,
}

pub async fn stats(State(state): State<AppState>) -> Result<Json<StatsResponse>, ApiError> {
    // Recorrer los diccionarios de términos es E/S bloqueante sobre el mmap.
    let response = tokio::task::spawn_blocking(move || {
        // Un único `Searcher`: `total` y `by_tipo` salen de la misma foto.
        let searcher = state.reader.searcher();
        count_by_tipo(&searcher, state.fields.tipo).map(|by_tipo| StatsResponse {
            total: searcher.num_docs(),
            by_tipo,
        })
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;

    Ok(Json(response))
}

/// Documentos vivos de cada `tipo`: sin los borrados o sustituidos que aún no
/// se han purgado al fusionar segmentos. Los `tipo` salen del diccionario de
/// términos de cada segmento (que sí conserva los de documentos borrados) y
/// cada uno se cuenta con una consulta, que descarta los borrados; un `tipo`
/// sin documentos vivos sólo sale si es uno de `KNOWN_TIPOS`.
pub fn count_by_tipo(searcher: &Searcher, tipo: Field) -> tantivy::Result<BTreeMap<String, u64>> {
    let mut tipos = BTreeSet::new();
    for segment in searcher.segment_readers() {
        let inverted_index = segment.inverted_index(tipo)?;
        let mut terms = inverted_index.terms().stream()?;
        while terms.advance() {
            tipos.insert(String::from_utf8_lossy(terms.key()).into_owned());
        }
    }

    let mut counts: BTreeMap<String, u64> = KNOWN_TIPOS
        .iter()
        .map(|known| (known.to_string(), 0))
        .collect();
    for name in tipos {
        let query = TermQuery::new(Term::from_field_text(tipo, &name), IndexRecordOption::Basic);
        let count = searcher.search(&query, &Count)? as u64;
        if count > 0 {
            counts.insert(name, count);
        }
    }
    Ok(counts)
}
