use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};
use tantivy::{
    Document, TantivyDocument, Term,
    collector::TopDocs,
    query::{
        BooleanQuery, ConstScoreQuery, Occur, Query as TantivyQuery, QueryParser, TermQuery,
        TermSetQuery,
    },
    schema::{IndexRecordOption, NamedFieldDocument},
};
use unicode_normalization::UnicodeNormalization;

use crate::acl::search_principals;
use crate::error::ApiError;
use crate::init::SearchFields;
use crate::state::AppState;

pub const DEFAULT_LIMIT: usize = 10;
/// Tope de `limit`: tantivy reserva memoria proporcional al límite pedido (un
/// `limit` enorme tumbaba el proceso entero por falta de memoria).
pub const MAX_LIMIT: usize = 1_000;
/// Tope de `offset`, por el mismo motivo: paginar a `offset` exige recolectar
/// `offset + limit` resultados.
pub const MAX_OFFSET: usize = 10_000;
/// Longitud máxima de `q`, en caracteres.
pub const MAX_QUERY_CHARS: usize = 1_000;
/// Anidamiento máximo de paréntesis que se entrega al parser de tantivy. Su
/// coste crece exponencialmente con la profundidad (×4 por nivel: 20 niveles
/// ya cuestan ~1 s de CPU y 30, días), así que una consulta como "((((…a" de
/// pocas decenas de caracteres bastaba para bloquear el servidor.
const MAX_QUERY_NESTING: usize = 8;

#[derive(Deserialize)]
pub struct SearchParams {
    pub q: String,
    pub tipo: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Principales separados por comas (`u:1,a:1`). Sin él, `public`.
    pub acl: Option<String>,
}

#[derive(Serialize)]
pub struct SearchResult {
    pub score: f32,
    pub doc: NamedFieldDocument,
}

pub async fn search_documents(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<Vec<SearchResult>>, ApiError> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let offset = params.offset.unwrap_or(0);
    if offset > MAX_OFFSET {
        return Err(ApiError::bad_request(format!(
            "`offset` no puede ser mayor que {MAX_OFFSET}"
        )));
    }
    if params.q.chars().count() > MAX_QUERY_CHARS {
        return Err(ApiError::bad_request(format!(
            "`q` no puede tener más de {MAX_QUERY_CHARS} caracteres"
        )));
    }
    let principals = search_principals(params.acl.as_deref())?;
    if limit == 0 {
        return Ok(Json(Vec::new()));
    }

    // Buscar y leer los documentos es E/S bloqueante sobre el índice mmap.
    let results = tokio::task::spawn_blocking(move || {
        run_search(
            &state,
            &params.q,
            params.tipo.as_deref(),
            &principals,
            limit,
            offset,
        )
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;

    Ok(Json(results))
}

fn run_search(
    state: &AppState,
    q: &str,
    tipo: Option<&str>,
    principals: &[String],
    limit: usize,
    offset: usize,
) -> tantivy::Result<Vec<SearchResult>> {
    let final_query = restrict_to(&state.fields, principals, base_query(state, q, tipo));

    let searcher = state.reader.searcher();
    let top_docs = searcher.search(
        &*final_query,
        &TopDocs::with_limit(limit).and_offset(offset),
    )?;

    let schema = state.index.schema();
    top_docs
        .into_iter()
        .map(|(score, doc_address)| {
            let retrieved_doc: TantivyDocument = searcher.doc(doc_address)?;
            Ok(SearchResult {
                score,
                doc: retrieved_doc.to_named_doc(&schema),
            })
        })
        .collect()
}

/// La consulta de v2: el texto de `q` y, si se pide, el filtro por `tipo`.
fn base_query(state: &AppState, q: &str, tipo: Option<&str>) -> Box<dyn TantivyQuery> {
    let text_query = parse_user_query(&state.query_parser, q);

    match tipo.map(str::trim) {
        Some(tipo_doc) if !tipo_doc.is_empty() => {
            let tipo_term = Term::from_field_text(state.fields.tipo, tipo_doc);

            let tipo_query: Box<dyn TantivyQuery> =
                Box::new(TermQuery::new(tipo_term, IndexRecordOption::Basic));

            Box::new(BooleanQuery::new(vec![
                (Occur::Must, text_query),
                (Occur::Must, tipo_query),
            ]))
        }

        _ => text_query,
    }
}

/// Deja en `query` sólo los documentos que comparten algún principal con
/// `principals`. Es una cláusula `Must` aparte, así que nada de lo que lleve
/// `q` (`*`, `acl:…`, `tipo:…`, `-x`) puede ampliar el resultado; y puntúa 0,
/// así que las puntuaciones son exactamente las de `query`.
fn restrict_to(
    fields: &SearchFields,
    principals: &[String],
    query: Box<dyn TantivyQuery>,
) -> Box<dyn TantivyQuery> {
    let terms = principals
        .iter()
        .map(|principal| Term::from_field_text(fields.acl, principal));
    let acl_filter = ConstScoreQuery::new(Box::new(TermSetQuery::new(terms)), 0.0);
    Box::new(BooleanQuery::new(vec![
        (Occur::Must, query),
        (Occur::Must, Box::new(acl_filter)),
    ]))
}

/// Interpreta `q` con la sintaxis de consultas de tantivy (frases entre
/// comillas, `+`/`-`, `AND`/`OR`, `campo:valor`…). No hace falta normalizar
/// mayúsculas ni tildes: el parser pasa cada término por el mismo analizador
/// que se usó al indexar.
///
/// Si `q` no es sintaxis válida —algo tan corriente como "12:30", una URL o
/// unas comillas sin cerrar— se busca como texto plano en lugar de responder
/// 400 al usuario. También se busca como texto plano si anida demasiados
/// paréntesis (ver `MAX_QUERY_NESTING`).
fn parse_user_query(parser: &QueryParser, q: &str) -> Box<dyn TantivyQuery> {
    if paren_depth(q) <= MAX_QUERY_NESTING
        && let Ok(query) = parser.parse_query(q)
    {
        return query;
    }
    parser.parse_query_lenient(&plain_text(q)).0
}

/// Profundidad máxima de paréntesis abiertos (por lo alto: cuenta también los
/// que van entre comillas).
fn paren_depth(q: &str) -> usize {
    let mut depth = 0usize;
    let mut max_depth = 0;
    for c in q.chars() {
        match c {
            '(' => {
                depth += 1;
                max_depth = max_depth.max(depth);
            }
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max_depth
}

/// Deja sólo letras y números; todo lo demás pasa a ser un separador, igual que
/// en el tokenizer. En minúsculas para que `AND`/`OR`/`NOT` no se tomen como
/// operadores.
fn plain_text(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for c in q.nfc() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else {
            out.push(' ');
        }
    }
    out
}
