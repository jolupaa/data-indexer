use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use crate::indexer::AppState;

use serde::{Deserialize, Serialize};


use tantivy::{
    collector::TopDocs,
    query::{
        BooleanQuery,
        Occur,
        Query as TantivyQuery,
        QueryParser,
        TermQuery,
    },
    schema::IndexRecordOption,
    TantivyDocument,
    Term,
    Document
};

use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

/// Normaliza el texto de la petición igual que el índice: a minúsculas y sin
/// tildes/diacríticos. Descompone en NFD (á → a + ´) y descarta las marcas
/// combinantes, dejando sólo el carácter base. Así "Actualización",
/// "actualizacion" y "ACTUALIZACIÓN" buscan exactamente lo mismo.
fn normalize_query(q: &str) -> String {
    q.nfd()
        .filter(|c| !is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
}

#[derive(Deserialize)]
pub struct SearchParams {
    pub q: String,
    pub tipo: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct SearchResult {
    pub score: f32,
    pub doc: serde_json::Value,
}

pub async fn search_documents(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> std::result::Result<Json<Vec<SearchResult>>, (StatusCode, String)> {
    let reader = state
        .index
        .reader()
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    let searcher = reader.searcher();

    let query_parser = QueryParser::for_index(
        &state.index,
        vec![
            state.fields.titulo,
            state.fields.subtitulo,
            state.fields.contenido,
        ],
    );

    let normalized_q = normalize_query(&params.q);

    let text_query = query_parser
        .parse_query(&normalized_q)
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;

    let final_query: Box<dyn TantivyQuery> = match params.tipo.as_deref() {
        Some(tipo_doc) if !tipo_doc.trim().is_empty() => {
            let tipo_term = Term::from_field_text(
                state.fields.tipo,
                tipo_doc.trim(),
            );

            let tipo_query: Box<dyn TantivyQuery> = Box::new(TermQuery::new(
                tipo_term,
                IndexRecordOption::Basic,
            ));

            Box::new(BooleanQuery::new(vec![
                (Occur::Must, text_query),
                (Occur::Must, tipo_query),
            ]))
        }

        _ => text_query,
    };

    let limit = params.limit.unwrap_or(10);

    let top_docs = searcher
        .search(&*final_query, &TopDocs::with_limit(limit))
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    let schema = state.index.schema();

    let mut results = Vec::new();

    for (score, doc_address) in top_docs {
        let retrieved_doc: TantivyDocument = searcher
            .doc(doc_address)
            .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

        let json_string = retrieved_doc.to_json(&schema);

        let doc_json: serde_json::Value = serde_json::from_str(&json_string)
            .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

        results.push(SearchResult {
            score,
            doc: doc_json,
        });
    }

    Ok(Json(results))
}