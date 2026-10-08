use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};
use tantivy::{
    Document, TantivyDocument, Term,
    collector::TopDocs,
    query::{
        BooleanQuery, ConstScoreQuery, Occur, PhrasePrefixQuery, Query as TantivyQuery,
        QueryParser, TermQuery, TermSetQuery,
    },
    schema::{IndexRecordOption, NamedFieldDocument},
    tokenizer::{TextAnalyzer, TokenStream},
};
use unicode_normalization::UnicodeNormalization;

use crate::acl::search_principals;
use crate::error::ApiError;
use crate::init::{SearchFields, es_analyzer, folding_analyzer};
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
/// Longitud mínima, en caracteres y ya analizado, del término que
/// `prefix=true` expande: "a" o "de" expandirían a medio índice.
pub const MIN_PREFIX_CHARS: usize = 3;
/// Máximo de términos del índice en los que se expande el prefijo, por campo y
/// segmento: acota el coste de un prefijo corto y corriente.
pub const PREFIX_MAX_EXPANSIONS: u32 = 200;

#[derive(Deserialize)]
pub struct SearchParams {
    pub q: String,
    pub tipo: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Principales separados por comas (`u:1,a:1`). Sin él, `public`.
    pub acl: Option<String>,
    /// `true`: la última palabra de `q` casa también como prefijo ("nomi" →
    /// "nómina"), para buscar mientras se escribe. Por defecto `false`.
    pub prefix: Option<bool>,
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
            params.prefix.unwrap_or(false),
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
    prefix: bool,
    limit: usize,
    offset: usize,
) -> tantivy::Result<Vec<SearchResult>> {
    let final_query = restrict_to(
        &state.fields,
        principals,
        base_query(state, q, tipo, prefix),
    );

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

/// La consulta de v2 —el texto de `q` y, si se pide, el filtro por `tipo`—,
/// con el prefijo de `prefix=true` como alternativa al texto.
fn base_query(
    state: &AppState,
    q: &str,
    tipo: Option<&str>,
    prefix: bool,
) -> Box<dyn TantivyQuery> {
    let mut text_query = parse_user_query(&state.query_parser, q);
    if prefix {
        text_query = with_prefix(&state.fields, text_query, q);
    }

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

/// Añade a `text_query`, como alternativa (`Should`), la última palabra de `q`
/// como prefijo en cada campo de texto. Cada campo que casa por prefijo suma
/// 1.0, y una palabra entera casa además por el texto, así que lo exacto queda
/// por delante. Sin un término de al menos `MIN_PREFIX_CHARS` caracteres
/// devuelve `text_query` tal cual.
fn with_prefix(
    fields: &SearchFields,
    text_query: Box<dyn TantivyQuery>,
    q: &str,
) -> Box<dyn TantivyQuery> {
    let Some(prefix) = last_prefix_term(q) else {
        return text_query;
    };
    let mut clauses = vec![(Occur::Should, text_query)];
    for field in fields.full_text() {
        let mut query = PhrasePrefixQuery::new(vec![Term::from_field_text(field, &prefix)]);
        query.set_max_expansions(PREFIX_MAX_EXPANSIONS);
        clauses.push((Occur::Should, Box::new(query)));
    }
    Box::new(BooleanQuery::new(clauses))
}

/// La última palabra de `q` como prefijo de los términos del índice, si tiene
/// al menos `MIN_PREFIX_CHARS` caracteres. Se toma de `plain_text(q)`, así que
/// la sintaxis de consulta (`"`, `-`, `campo:`) no cuenta como palabra.
///
/// Pasa por el analizador del índice (minúsculas, sin tildes, en singular).
/// Su filtro de plurales sólo recorta ("nominas" → "nomina"), y lo recortado
/// sigue siendo un prefijo de lo tecleado, salvo por la `z` final, que pasa a
/// `c` ("luz" → "luc"). Eso vale para una palabra entera, pero una palabra a
/// medias como "plaz" sigue por la `z` ("plaza"), no por la `c` ("placa"); en
/// ese caso el prefijo es la palabra sin pasar a singular. La palabra entera
/// ("luz" y su plural "luces") ya la encuentra el texto de `q`.
fn last_prefix_term(q: &str) -> Option<String> {
    let plain = plain_text(q);
    let last_word = plain.split_whitespace().last()?;
    let folded = last_token(&mut folding_analyzer(), last_word)?;
    let stemmed = last_token(&mut es_analyzer(), last_word)?;
    let term = if folded.starts_with(&stemmed) {
        stemmed
    } else {
        folded
    };
    Some(term).filter(|term| term.chars().count() >= MIN_PREFIX_CHARS)
}

/// El último término que produce `analyzer` para `text`.
fn last_token(analyzer: &mut TextAnalyzer, text: &str) -> Option<String> {
    let mut stream = analyzer.token_stream(text);
    let mut token = None;
    while stream.advance() {
        token = Some(stream.token().text.clone());
    }
    token
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prefix_is_the_last_word_analysed_like_the_index() {
        assert_eq!(last_prefix_term("nomi").as_deref(), Some("nomi"));
        assert_eq!(last_prefix_term("calendario TUR").as_deref(), Some("tur"));
        assert_eq!(last_prefix_term("Nóminas").as_deref(), Some("nomina"));
        assert_eq!(last_prefix_term("No\u{301}mi").as_deref(), Some("nomi"));
        assert_eq!(last_prefix_term("nomi   ").as_deref(), Some("nomi"));
        assert_eq!(last_prefix_term("\"turnos de oct").as_deref(), Some("oct"));
        assert_eq!(last_prefix_term("tipo:chat_msg").as_deref(), Some("msg"));
    }

    #[test]
    fn a_partial_word_ending_in_z_keeps_its_z() {
        // En una palabra entera la `z` final pasa a `c` ("luz" → "luc"), pero
        // a medias la palabra sigue por la `z` ("plaz" → "plaza").
        assert_eq!(last_prefix_term("actualiz").as_deref(), Some("actualiz"));
        assert_eq!(last_prefix_term("organiz").as_deref(), Some("organiz"));
        assert_eq!(last_prefix_term("AUTORIZ").as_deref(), Some("autoriz"));
        assert_eq!(last_prefix_term("plaz").as_deref(), Some("plaz"));
        // Lo que el filtro de plurales sólo recorta sigue recortado.
        assert_eq!(last_prefix_term("nominas").as_deref(), Some("nomina"));
        assert_eq!(last_prefix_term("plazas").as_deref(), Some("plaza"));
        assert_eq!(
            last_prefix_term("actualizaciones").as_deref(),
            Some("actualizacion")
        );
    }

    #[test]
    fn short_or_missing_words_have_no_prefix() {
        for q in [
            "",
            "   ",
            "no",
            "turnos de",
            "*",
            "-",
            "¿?",
            &"a".repeat(41),
        ] {
            assert_eq!(last_prefix_term(q), None, "{q:?}");
        }
    }
}
