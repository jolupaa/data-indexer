//! Pruebas de la API HTTP completa contra un índice en memoria.

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tantivy::{
    Index, TantivyDocument, Term,
    collector::TopDocs,
    query::{BooleanQuery, Occur, Query, QueryParser, TermQuery},
    schema::{IndexRecordOption, Value as _},
};
use tower::ServiceExt;

use crate::init::{build_schema, register_tokenizers};
use crate::searcher::MAX_QUERY_CHARS;
use crate::server::router;
use crate::state::AppState;

fn test_state() -> AppState {
    let (schema, fields) = build_schema();
    let index = Index::create_in_ram(schema);
    register_tokenizers(&index);
    AppState::new(index, fields).unwrap()
}

fn test_app() -> Router {
    router(test_state())
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => builder.body(Body::empty()),
    }
    .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn upsert(app: &Router, doc: Value) {
    let (status, body) = send(app, Method::POST, "/index/upsert", Some(doc)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn search(app: &Router, query: &str) -> Vec<Value> {
    let (status, body) = send(app, Method::GET, &format!("/search?{query}"), None).await;
    assert_eq!(status, StatusCode::OK, "{query}: {body}");
    serde_json::from_str(&body).unwrap()
}

fn noticia(id: &str, titulo: &str) -> Value {
    json!({
        "id": id,
        "tipo": "noticia",
        "titulo": titulo,
        "subtitulo": "",
        "contenido": "",
        "fecha": "2026-06-16",
    })
}

fn chat(id: &str, thread: &str, contenido: &str, acl: &[&str]) -> Value {
    json!({
        "id": id,
        "tipo": "chat_msg",
        "thread": thread,
        "contenido": contenido,
        "fecha": "2026-10-03T08:42:00",
        "acl": acl,
    })
}

fn ids(results: &[Value]) -> Vec<String> {
    let mut ids: Vec<String> = results
        .iter()
        .map(|r| r["doc"]["id"][0].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn upsert_is_searchable_immediately_and_replaces_by_uid() {
    let app = test_app();
    upsert(&app, noticia("1", "Primera versión")).await;
    assert_eq!(ids(&search(&app, "q=primera").await), ["1"]);

    upsert(&app, noticia("1", "Segunda versión")).await;
    assert!(search(&app, "q=primera").await.is_empty());
    let results = search(&app, "q=segunda").await;
    assert_eq!(results.len(), 1);
    // La forma de `doc` no cambia: cada campo guardado es un array.
    assert_eq!(results[0]["doc"]["titulo"], json!(["Segunda versión"]));
    assert_eq!(results[0]["doc"]["uid"], json!(["noticia:1"]));
    assert!(results[0]["doc"].get("contenido").is_none());
    // Igual que las noticias de `reindex`: sin `info_title`.
    assert!(results[0]["doc"].get("info_title").is_none());
}

#[tokio::test]
async fn delete_removes_the_document() {
    let app = test_app();
    upsert(&app, noticia("7", "Borrable")).await;
    let (status, _) = send(
        &app,
        Method::DELETE,
        "/index/delete",
        Some(json!({"id": "7", "tipo": "noticia"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(search(&app, "q=borrable").await.is_empty());
}

#[tokio::test]
async fn limit_zero_and_huge_limits_do_not_crash() {
    let app = test_app();
    upsert(&app, noticia("1", "hola")).await;

    // limit=0 hacía saltar un assert de tantivy.
    assert!(search(&app, "q=hola&limit=0").await.is_empty());
    // Un limit enorme abortaba el proceso por falta de memoria.
    assert_eq!(search(&app, "q=hola&limit=1099511627776").await.len(), 1);

    let (status, _) = send(&app, Method::GET, "/search?q=hola&offset=99999999", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn offset_paginates() {
    let app = test_app();
    for id in ["1", "2", "3"] {
        upsert(&app, noticia(id, "paginación")).await;
    }
    let first = search(&app, "q=paginacion&limit=2").await;
    let rest = search(&app, "q=paginacion&limit=2&offset=2").await;
    assert_eq!(first.len(), 2);
    assert_eq!(rest.len(), 1);
    let mut all = ids(&first);
    all.extend(ids(&rest));
    all.sort();
    assert_eq!(all, ["1", "2", "3"]);
}

#[tokio::test]
async fn everyday_text_that_is_not_valid_syntax_still_searches() {
    let app = test_app();
    upsert(
        &app,
        noticia("1", "Reunión a las 12:30 en https://example.com"),
    )
    .await;

    for q in [
        "12%3A30",
        "https%3A%2F%2Fexample.com",
        "%22reuni%C3%B3n",
        "reuni%C3%B3n)",
        "reunion%20AND",
    ] {
        assert_eq!(ids(&search(&app, &format!("q={q}")).await), ["1"], "q={q}");
    }
}

#[tokio::test]
async fn query_syntax_keeps_working() {
    let app = test_app();
    upsert(&app, noticia("1", "casa roja")).await;
    upsert(&app, noticia("2", "casa azul")).await;

    // AND/OR/NOT dejaban de funcionar al pasar la consulta a minúsculas.
    assert_eq!(ids(&search(&app, "q=casa%20AND%20roja").await), ["1"]);
    assert_eq!(ids(&search(&app, "q=casa%20-roja").await), ["2"]);
    assert_eq!(ids(&search(&app, "q=%22casa%20azul%22").await), ["2"]);
    assert_eq!(ids(&search(&app, "q=titulo%3Aroja").await), ["1"]);
}

#[tokio::test]
async fn accents_case_plurals_and_decomposed_text_match() {
    let app = test_app();
    upsert(&app, noticia("1", "Las clases del presidente")).await;
    // Texto en NFD ("o" + tilde combinante), típico de texto extraído de PDF.
    upsert(&app, noticia("2", "Actualizacio\u{301}n del sistema")).await;

    assert_eq!(ids(&search(&app, "q=CLASE").await), ["1"]);
    assert_eq!(ids(&search(&app, "q=presidentes").await), ["1"]);
    assert_eq!(ids(&search(&app, "q=actualizaciones").await), ["2"]);
    assert_eq!(ids(&search(&app, "q=Actualizaci%C3%B3n").await), ["2"]);
}

#[tokio::test]
async fn tipo_filter_and_info_title() {
    let app = test_app();
    upsert(&app, noticia("1", "presupuesto")).await;
    upsert(
        &app,
        json!({
            "id": "1",
            "tipo": "info_doc",
            "titulo": "Documento",
            "info_title": "Presupuestos anuales",
        }),
    )
    .await;

    // Mismo id con distinto tipo: no se pisan.
    assert_eq!(search(&app, "q=presupuesto").await.len(), 2);

    let info = search(&app, "q=presupuesto&tipo=%20info_doc%20").await;
    assert_eq!(info.len(), 1);
    assert_eq!(info[0]["doc"]["tipo"], json!(["info_doc"]));
    // info_title se indexaba pero no se buscaba ni podía enviarse por la API.
    assert_eq!(
        info[0]["doc"]["info_title"],
        json!(["Presupuestos anuales"])
    );

    // `*` con `tipo` lista la categoría entera.
    assert_eq!(ids(&search(&app, "q=*&tipo=noticia").await), ["1"]);
    assert_eq!(search(&app, "q=*").await.len(), 2);
}

#[tokio::test]
async fn upsert_validates_the_uid_parts_and_accepts_nulls() {
    let app = test_app();
    for bad in [
        json!({"id": "1", "tipo": ""}),
        json!({"id": " ", "tipo": "noticia"}),
        json!({"id": "c", "tipo": "a:b"}),
        json!({"id": "x".repeat(70_000), "tipo": "noticia"}),
    ] {
        let (status, _) = send(&app, Method::POST, "/index/upsert", Some(bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    upsert(
        &app,
        json!({"id": "9", "tipo": "noticia", "titulo": "nulos", "subtitulo": null, "fecha": null}),
    )
    .await;
    assert_eq!(ids(&search(&app, "q=nulos").await), ["9"]);
}

#[tokio::test]
async fn batch_upsert_indexes_everything_in_one_commit() {
    let app = test_app();
    let (status, body) = send(
        &app,
        Method::POST,
        "/index/upsert/batch",
        Some(json!([
            noticia("1", "lote"),
            noticia("2", "lote"),
            noticia("1", "lote final")
        ])),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, r#"{"ok":true,"indexed":3}"#);

    // El último upsert de un mismo uid es el que queda.
    let results = search(&app, "q=lote").await;
    assert_eq!(ids(&results), ["1", "2"]);
    assert_eq!(ids(&search(&app, "q=final").await), ["1"]);

    // Un documento inválido rechaza el lote entero, e indica cuál es.
    let (status, body) = send(
        &app,
        Method::POST,
        "/index/upsert/batch",
        Some(json!([noticia("3", "nuevo"), {"id": "4", "tipo": ""}])),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.starts_with("[1]: "), "{body}");
    assert!(search(&app, "q=nuevo").await.is_empty());
}

#[tokio::test]
async fn accepts_bodies_larger_than_axum_default() {
    let app = test_app();
    let mut doc = noticia("big", "grande");
    doc["contenido"] = json!("palabra ".repeat(500_000)); // ~4 MB
    upsert(&app, doc).await;
    assert_eq!(ids(&search(&app, "q=palabras").await), ["big"]);
}

#[tokio::test]
async fn only_upserts_accept_large_bodies() {
    let app = test_app();
    let big_delete = json!({"id": "x".repeat(3_000_000), "tipo": "noticia"});
    let (status, _) = send(&app, Method::DELETE, "/index/delete", Some(big_delete)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn health_reports_document_count() {
    let app = test_app();
    upsert(&app, noticia("1", "uno")).await;
    let (status, body) = send(&app, Method::GET, "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"ok":true,"docs":1}"#);
}

#[tokio::test]
async fn deeply_nested_queries_do_not_hang_the_server() {
    let app = test_app();
    upsert(&app, noticia("1", "hola")).await;

    // Sin tope de anidamiento, el parser tardaría días en esta consulta.
    let nested = format!("{}hola{}", "(".repeat(40), ")".repeat(40));
    let unbalanced = "(hola ".repeat(150);
    for q in [nested, unbalanced] {
        let uri = format!(
            "q={}",
            q.replace('(', "%28")
                .replace(')', "%29")
                .replace(' ', "%20")
        );
        assert_eq!(ids(&search(&app, &uri).await), ["1"]);
    }

    // Unos pocos niveles siguen siendo sintaxis normal.
    assert!(search(&app, "q=%28%28hola%29%29%20-hola").await.is_empty());

    let too_long = format!("q={}", "a".repeat(MAX_QUERY_CHARS + 1));
    let (status, _) = send(&app, Method::GET, &format!("/search?{too_long}"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upsert_rejects_invalid_acl_and_thread() {
    let app = test_app();
    let too_many: Vec<String> = (0..33).map(|i| format!("u:{i}")).collect();
    for bad in [
        json!({"id": "1", "tipo": "chat_msg", "acl": [""]}),
        json!({"id": "1", "tipo": "chat_msg", "acl": ["x".repeat(257)]}),
        json!({"id": "1", "tipo": "chat_msg", "acl": ["u:a b"]}),
        json!({"id": "1", "tipo": "chat_msg", "acl": ["u:a,b"]}),
        json!({"id": "1", "tipo": "chat_msg", "acl": too_many}),
        json!({"id": "1", "tipo": "chat_msg", "thread": "   "}),
        json!({"id": "1", "tipo": "chat_msg", "thread": "t".repeat(1025)}),
    ] {
        let (status, body) = send(&app, Method::POST, "/index/upsert", Some(bad.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }

    // `acl` es una lista de strings, no un string.
    let (status, _) = send(
        &app,
        Method::POST,
        "/index/upsert",
        Some(json!({"id": "1", "tipo": "chat_msg", "acl": "u:1"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // 32 valores de 256 bytes justos sí valen.
    let at_the_limit: Vec<String> = (0..32)
        .map(|i| format!("u:{i:02}{}", "x".repeat(252)))
        .collect();
    upsert(
        &app,
        json!({"id": "2", "tipo": "chat_msg", "acl": at_the_limit}),
    )
    .await;
}

#[tokio::test]
async fn batch_names_the_item_with_an_invalid_acl() {
    let app = test_app();
    let (status, body) = send(
        &app,
        Method::POST,
        "/index/upsert/batch",
        Some(json!([
            {"id": "1", "tipo": "chat_msg", "acl": ["u:1"], "contenido": "lote"},
            {"id": "2", "tipo": "chat_msg", "acl": ["u:1 "], "contenido": "lote"},
        ])),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.starts_with("[1]: "), "{body}");
}

#[tokio::test]
async fn results_carry_thread_but_never_acl() {
    let app = test_app();
    // Sin `acl` es público, así que la búsqueda por defecto lo encuentra.
    upsert(
        &app,
        json!({
            "id": "7101",
            "tipo": "chat_msg",
            "thread": "dev-thread-maria",
            "contenido": "calendario",
        }),
    )
    .await;
    let results = search(&app, "q=calendario").await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["doc"]["thread"], json!(["dev-thread-maria"]));
    assert!(results[0]["doc"].get("acl").is_none());

    upsert(&app, noticia("1", "sin hilo")).await;
    let results = search(&app, "q=hilo").await;
    assert_eq!(results.len(), 1);
    assert!(results[0]["doc"].get("thread").is_none());
}

#[tokio::test]
async fn acl_hides_documents_without_a_matching_principal() {
    let app = test_app();
    upsert(&app, noticia("1", "turnos de octubre")).await;
    upsert(
        &app,
        chat(
            "7101",
            "t-ana",
            "turnos de octubre",
            &["u:usr-ana", "a:usr-laura"],
        ),
    )
    .await;
    upsert(
        &app,
        chat("7102", "t-bob", "turnos de octubre", &["u:usr-bob"]),
    )
    .await;

    // Sin `acl`, sólo lo público.
    assert_eq!(ids(&search(&app, "q=turnos").await), ["1"]);
    assert_eq!(ids(&search(&app, "q=turnos&acl=public").await), ["1"]);
    // Cada principal ve lo suyo y nada más.
    assert_eq!(ids(&search(&app, "q=turnos&acl=u:usr-ana").await), ["7101"]);
    assert_eq!(
        ids(&search(&app, "q=turnos&acl=a:usr-laura").await),
        ["7101"]
    );
    assert_eq!(ids(&search(&app, "q=turnos&acl=u:usr-bob").await), ["7102"]);
    assert!(search(&app, "q=turnos&acl=u:usr-carla").await.is_empty());
    // Varios principales: lo que ve cualquiera de ellos.
    assert_eq!(
        ids(&search(&app, "q=turnos&acl=u:usr-ana,public").await),
        ["1", "7101"]
    );
    assert_eq!(
        ids(&search(&app, "q=turnos&acl=u:usr-ana,a:usr-ana").await),
        ["7101"]
    );

    // El resultado lleva el thread y nunca la acl.
    let results = search(&app, "q=turnos&acl=u:usr-ana").await;
    assert_eq!(results[0]["doc"]["thread"], json!(["t-ana"]));
    assert!(results[0]["doc"].get("acl").is_none());
}

#[tokio::test]
async fn query_syntax_cannot_widen_access() {
    let app = test_app();
    upsert(&app, noticia("1", "turnos")).await;
    upsert(&app, chat("7101", "t-ana", "turnos", &["u:usr-ana"])).await;

    for q in [
        "*",
        "turnos",
        "acl:u%3Ausr-ana",
        "acl:%22u%3Ausr-ana%22",
        "acl:public",
        "tipo:chat_msg",
        "thread:t-ana",
        "-x",
        "turnos%20OR%20acl:%22u%3Ausr-ana%22",
        "*%20OR%20tipo:chat_msg",
        "%2Bturnos%20-acl:public",
    ] {
        for extra in ["", "&acl=public", "&acl=u:usr-bob", "&tipo=chat_msg"] {
            let uri = format!("q={q}{extra}");
            let found = ids(&search(&app, &uri).await);
            assert!(!found.contains(&"7101".to_string()), "{uri}: {found:?}");
        }
    }
}

#[tokio::test]
async fn search_validates_acl() {
    let app = test_app();
    upsert(&app, noticia("1", "hola")).await;
    let too_many = vec!["u:1"; 65].join(",");
    let too_long = "x".repeat(257);
    for bad in [
        "acl=".to_string(),
        "acl=u:1,".to_string(),
        "acl=u:1,,u:2".to_string(),
        "acl=u%3Aa%20b".to_string(),
        "acl=u:1&acl=u:2".to_string(),
        format!("acl={too_many}"),
        format!("acl={too_long}"),
        // Aunque `limit=0` no busque nada, un `acl` inválido es un error.
        "acl=&limit=0".to_string(),
    ] {
        let uri = format!("/search?q=hola&{bad}");
        let (status, body) = send(&app, Method::GET, &uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
    }

    let mut at_the_limit = vec!["u:1"; 63];
    at_the_limit.push("public");
    let uri = format!("q=hola&acl={}", at_the_limit.join(","));
    assert_eq!(ids(&search(&app, &uri).await), ["1"]);
}

/// (id, puntuación) de la consulta de v2 —el texto y, si hay, el filtro por
/// `tipo`— ejecutada directamente sobre el índice, sin ACL. La puntuación va
/// como la escribe serde_json, para comparar los f32 bit a bit.
fn v2_scores(state: &AppState, q: &str, tipo: Option<&str>) -> Vec<(String, String)> {
    let parser = QueryParser::for_index(&state.index, state.fields.full_text());
    let text = parser.parse_query(q).unwrap();
    let query: Box<dyn Query> = match tipo {
        Some(tipo) => Box::new(BooleanQuery::new(vec![
            (Occur::Must, text),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(state.fields.tipo, tipo),
                    IndexRecordOption::Basic,
                )),
            ),
        ])),
        None => text,
    };
    let searcher = state.reader.searcher();
    searcher
        .search(&*query, &TopDocs::with_limit(100))
        .unwrap()
        .into_iter()
        .map(|(score, address)| {
            let doc: TantivyDocument = searcher.doc(address).unwrap();
            let id = doc.get_first(state.fields.id).unwrap().as_str().unwrap();
            (id.to_string(), serde_json::to_string(&score).unwrap())
        })
        .collect()
}

fn scores(results: &[Value]) -> Vec<(String, String)> {
    results
        .iter()
        .map(|r| {
            let id = r["doc"]["id"][0].as_str().unwrap().to_string();
            (id, r["score"].to_string())
        })
        .collect()
}

#[tokio::test]
async fn without_acl_results_and_scores_are_those_of_v2() {
    let state = test_state();
    let app = router(state.clone());
    upsert(&app, noticia("1", "Presupuesto anual")).await;
    upsert(&app, noticia("2", "Presupuesto")).await;
    upsert(
        &app,
        noticia("3", "Presupuesto del presupuesto anual de la empresa"),
    )
    .await;
    upsert(
        &app,
        json!({"id": "4", "tipo": "info_doc", "titulo": "Anual", "info_title": "Presupuestos"}),
    )
    .await;
    // Documentos privados que también casan: cuentan en las estadísticas de
    // BM25 igual que en v2, pero no salen.
    upsert(&app, chat("c1", "t1", "presupuesto anual", &["u:usr-ana"])).await;
    upsert(&app, chat("c2", "t1", "presupuesto", &["u:usr-ana"])).await;

    for (q, uri_q, tipo) in [
        ("presupuesto", "presupuesto", None),
        ("presupuesto anual", "presupuesto%20anual", None),
        ("\"presupuesto anual\"", "%22presupuesto%20anual%22", None),
        ("titulo:presupuesto", "titulo:presupuesto", None),
        ("presupuesto -empresa", "presupuesto%20-empresa", None),
        ("*", "*", None),
        ("presupuesto", "presupuesto", Some("noticia")),
        ("*", "*", Some("info_doc")),
    ] {
        let expected: Vec<(String, String)> = v2_scores(&state, q, tipo)
            .into_iter()
            .filter(|(id, _)| !id.starts_with('c'))
            .collect();
        assert!(!expected.is_empty(), "{q}");
        let tipo_param = tipo.map(|t| format!("&tipo={t}")).unwrap_or_default();
        for acl_param in ["", "&acl=public"] {
            let uri = format!("q={uri_q}&limit=100{tipo_param}{acl_param}");
            assert_eq!(scores(&search(&app, &uri).await), expected, "{uri}");
        }
    }
}

#[tokio::test]
async fn reupserting_with_a_new_acl_revokes_the_old_principals() {
    let app = test_app();
    upsert(
        &app,
        chat("7101", "t1", "turnos", &["u:usr-ana", "a:usr-laura"]),
    )
    .await;

    // Reasignación: el hilo pasa de Laura a Pedro.
    upsert(
        &app,
        chat("7101", "t1", "turnos", &["u:usr-ana", "a:usr-pedro"]),
    )
    .await;
    assert!(search(&app, "q=turnos&acl=a:usr-laura").await.is_empty());
    assert_eq!(
        ids(&search(&app, "q=turnos&acl=a:usr-pedro").await),
        ["7101"]
    );

    // Lo mismo por lotes.
    let (status, body) = send(
        &app,
        Method::POST,
        "/index/upsert/batch",
        Some(json!([chat(
            "7101",
            "t1",
            "turnos",
            &["u:usr-ana", "a:usr-marta"]
        )])),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(search(&app, "q=turnos&acl=a:usr-pedro").await.is_empty());
    assert_eq!(
        ids(&search(&app, "q=turnos&acl=a:usr-marta").await),
        ["7101"]
    );
    assert_eq!(ids(&search(&app, "q=turnos&acl=u:usr-ana").await), ["7101"]);
}

#[tokio::test]
async fn principals_match_exactly() {
    let app = test_app();
    upsert(&app, chat("1", "t1", "turnos", &["u:12"])).await;
    upsert(&app, chat("2", "t2", "turnos", &["u:ABC"])).await;

    assert_eq!(ids(&search(&app, "q=turnos&acl=u:12").await), ["1"]);
    assert_eq!(ids(&search(&app, "q=turnos&acl=u:ABC").await), ["2"]);
    // Ni prefijos, ni mayúsculas/minúsculas, ni otro rol con el mismo id.
    for acl in ["u:1", "u:123", "u:", "u", "a:12", "u:abc", "U:ABC", "u:AB"] {
        let uri = format!("q=turnos&acl={acl}");
        assert!(search(&app, &uri).await.is_empty(), "{uri}");
    }
}

#[tokio::test]
async fn prefix_expands_only_the_last_word() {
    let app = test_app();
    upsert(&app, noticia("1", "Nómina de septiembre")).await;
    upsert(&app, noticia("2", "Recibo de nóminas pendientes")).await;

    // Sin `prefix`, como en v2: una palabra a medias no casa.
    assert!(search(&app, "q=nomi").await.is_empty());
    assert!(search(&app, "q=nomi&prefix=false").await.is_empty());

    assert_eq!(ids(&search(&app, "q=nomi&prefix=true").await), ["1", "2"]);
    assert_eq!(ids(&search(&app, "q=septiem&prefix=true").await), ["1"]);
    // Sólo la última palabra es un prefijo: "nomi" no casa como palabra.
    assert_eq!(ids(&search(&app, "q=nomi%20sept&prefix=true").await), ["1"]);
    // Con menos de 3 caracteres no se expande.
    assert!(search(&app, "q=no&prefix=true").await.is_empty());

    let (status, _) = send(&app, Method::GET, "/search?q=nomi&prefix=si", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn exact_matches_rank_above_prefix_only_matches() {
    let app = test_app();
    upsert(&app, noticia("1", "turnos")).await;
    upsert(&app, noticia("2", "turn")).await;

    let results = search(&app, "q=turn&prefix=true").await;
    assert_eq!(ids(&results), ["1", "2"]);
    assert_eq!(results[0]["doc"]["id"], json!(["2"]));
}

#[tokio::test]
async fn prefix_matches_respect_acl_and_tipo() {
    let app = test_app();
    upsert(&app, noticia("1", "turnos")).await;
    upsert(
        &app,
        chat("7101", "t1", "turnos de octubre", &["u:usr-ana"]),
    )
    .await;

    assert_eq!(ids(&search(&app, "q=tur&prefix=true").await), ["1"]);
    assert_eq!(
        ids(&search(&app, "q=tur&prefix=true&acl=u:usr-ana").await),
        ["7101"]
    );
    assert!(
        search(&app, "q=tur&prefix=true&acl=u:usr-bob")
            .await
            .is_empty()
    );
    assert_eq!(
        ids(&search(&app, "q=tur&prefix=true&acl=u:usr-ana,public&tipo=noticia").await),
        ["1"]
    );
}

#[tokio::test]
async fn prefix_ignores_case_accents_and_a_trailing_space() {
    let app = test_app();
    upsert(&app, noticia("1", "Nómina de septiembre")).await;

    for q in [
        "NOMI",
        "N%C3%B3mi",  // "Nómi"
        "No%CC%81mi", // "Nómi" en NFD
        "nomi%20",    // espacio al final
        "N%C3%93MINAS",
        "%22nomi", // comillas sin cerrar
    ] {
        let uri = format!("q={q}&prefix=true");
        assert_eq!(ids(&search(&app, &uri).await), ["1"], "{uri}");
    }
}

#[tokio::test]
async fn prefix_with_nothing_to_expand_returns_nothing() {
    let app = test_app();
    upsert(&app, noticia("1", "turnos")).await;
    upsert(&app, chat("7101", "t1", "turnos", &["u:usr-ana"])).await;

    for q in ["", "%20%20", "%22", "-", "%3A%3A", "tu", "%C2%BF%3F"] {
        for acl in ["", "&acl=u:usr-ana"] {
            let uri = format!("q={q}&prefix=true{acl}");
            assert!(search(&app, &uri).await.is_empty(), "{uri}");
        }
    }
}

#[tokio::test]
async fn delete_thread_removes_the_whole_conversation() {
    let app = test_app();
    upsert(&app, noticia("1", "turnos")).await;
    for (id, thread) in [("7101", "t-ana"), ("7102", "t-ana"), ("7201", "t-bob")] {
        upsert(
            &app,
            chat(id, thread, "turnos", &["u:usr-ana", "u:usr-bob"]),
        )
        .await;
    }
    let everything = "q=turnos&acl=u:usr-ana,u:usr-bob,public";

    let (status, body) = send(
        &app,
        Method::DELETE,
        "/index/delete/thread",
        Some(json!({"thread": "t-ana"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, r#"{"ok":true}"#);
    assert_eq!(ids(&search(&app, everything).await), ["1", "7201"]);

    // Una conversación que no existe no es un error, y el thread se compara
    // tal cual: " t-bob " no es "t-bob".
    for thread in ["no-existe", " t-bob "] {
        let (status, _) = send(
            &app,
            Method::DELETE,
            "/index/delete/thread",
            Some(json!({ "thread": thread })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(ids(&search(&app, everything).await), ["1", "7201"]);
}

#[tokio::test]
async fn delete_tipo_removes_a_whole_corpus() {
    let app = test_app();
    upsert(&app, noticia("1", "turnos")).await;
    upsert(
        &app,
        json!({"id": "1", "tipo": "info_doc", "titulo": "turnos"}),
    )
    .await;
    upsert(&app, chat("7101", "t-ana", "turnos", &["u:usr-ana"])).await;
    upsert(&app, chat("7201", "t-bob", "turnos", &["u:usr-bob"])).await;

    // El tipo se recorta, como al indexar.
    let (status, body) = send(
        &app,
        Method::DELETE,
        "/index/delete/tipo",
        Some(json!({"tipo": " chat_msg "})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, r#"{"ok":true}"#);

    let results = search(&app, "q=turnos&acl=u:usr-ana,u:usr-bob,public").await;
    let mut tipos: Vec<&str> = results
        .iter()
        .map(|r| r["doc"]["tipo"][0].as_str().unwrap())
        .collect();
    tipos.sort();
    assert_eq!(tipos, ["info_doc", "noticia"]);
}

#[tokio::test]
async fn deletes_by_thread_and_tipo_validate_their_input() {
    let app = test_app();
    for (uri, bad) in [
        ("/index/delete/thread", json!({"thread": ""})),
        ("/index/delete/thread", json!({"thread": "   "})),
        ("/index/delete/thread", json!({"thread": "t".repeat(1025)})),
        ("/index/delete/tipo", json!({"tipo": ""})),
        ("/index/delete/tipo", json!({"tipo": "   "})),
        ("/index/delete/tipo", json!({"tipo": "a:b"})),
        ("/index/delete/tipo", json!({"tipo": "t".repeat(1025)})),
    ] {
        let (status, body) = send(&app, Method::DELETE, uri, Some(bad.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} {bad}: {body}");
    }
    for (uri, bad) in [
        ("/index/delete/thread", json!({})),
        ("/index/delete/thread", json!({"thread": 7})),
        ("/index/delete/tipo", json!({"thread": "t-ana"})),
    ] {
        let (status, _) = send(&app, Method::DELETE, uri, Some(bad.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri} {bad}");
    }
    let (status, _) = send(
        &app,
        Method::POST,
        "/index/delete/thread",
        Some(json!({"thread": "t-ana"})),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}
