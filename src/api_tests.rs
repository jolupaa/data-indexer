//! Pruebas de la API HTTP completa contra un índice en memoria.

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tantivy::Index;
use tower::ServiceExt;

use crate::init::{build_schema, register_tokenizers};
use crate::searcher::MAX_QUERY_CHARS;
use crate::server::router;
use crate::state::AppState;

fn test_app() -> Router {
    let (schema, fields) = build_schema();
    let index = Index::create_in_ram(schema);
    register_tokenizers(&index);
    router(AppState::new(index, fields).unwrap())
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

    // Un documento inválido rechaza el lote entero.
    let (status, _) = send(
        &app,
        Method::POST,
        "/index/upsert/batch",
        Some(json!([noticia("3", "nuevo"), {"id": "4", "tipo": ""}])),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
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
