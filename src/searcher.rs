use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};

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
};

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    tipo: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct SearchResult {
    score: f32,
    doc: serde_json::Value,
}

async fn search_documents(
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

    let text_query = query_parser
        .parse_query(&params.q)
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