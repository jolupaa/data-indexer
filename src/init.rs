use anyhow::Result;
use sqlx::PgPool;
use std::path::Path;
use tantivy::{
    doc,
    schema::{
        Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, STORED, STRING,
    },
    tokenizer::{
        AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer,
    },
    Index,
};

use crate::spanish_plural::SpanishPluralFilter;

/// Nombre del tokenizer que pliega acentos (á→a, ñ→n, …) además de pasar a
/// minúsculas. Se aplica a los campos de texto buscables para que "actualizacion"
/// encuentre "actualización". Debe registrarse en cada Index que se abra (tanto
/// al reindexar como al servir) con `register_tokenizers`.
pub const ES_TOKENIZER: &str = "es_folding";

use crate::utils::*;


#[derive(sqlx::FromRow)]
struct Noticia {
    id: String,
    titulo: String,
    subtitulo: String,
    contenido: String,
    fecha: String,
}

#[derive(sqlx::FromRow)]
struct InfoDoc {
    id: String,
    info_title: String,
    title: String,
    contenido: String,
    subtitle: String,
    created_at: String,
}

#[derive(Clone, Copy)]
pub struct SearchFields {
    pub id: Field,
    pub uid: Field,
    pub tipo: Field,
    pub info_title: Field,
    pub titulo: Field,
    pub subtitulo: Field,
    pub contenido: Field,
    pub fecha: Field,
}

/// Opciones de un campo de texto buscable con el tokenizer que pliega acentos.
/// `stored` controla si el valor original se devuelve en los resultados.
fn folded_text(stored: bool) -> TextOptions {
    let indexing = TextFieldIndexing::default()
        .set_tokenizer(ES_TOKENIZER)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let opts = TextOptions::default().set_indexing_options(indexing);
    if stored {
        opts.set_stored()
    } else {
        opts
    }
}

/// Construye el esquema del índice y devuelve los handles de cada campo.
/// Es la única fuente de verdad del esquema: la usan tanto `reindex` (al crear
/// el índice) como el servidor (para reconstruir los `Field` en el mismo orden).
pub fn build_schema() -> (Schema, SearchFields) {
    let mut schema_builder = Schema::builder();

    let fields = SearchFields {
        id: schema_builder.add_text_field("id", STRING | STORED),
        uid: schema_builder.add_text_field("uid", STRING | STORED),
        tipo: schema_builder.add_text_field("tipo", STRING | STORED),
        info_title: schema_builder.add_text_field("info_title", folded_text(true)),
        titulo: schema_builder.add_text_field("titulo", folded_text(true)),
        subtitulo: schema_builder.add_text_field("subtitulo", folded_text(true)),
        contenido: schema_builder.add_text_field("contenido", folded_text(false)),
        fecha: schema_builder.add_text_field("fecha", STRING | STORED),
    };

    (schema_builder.build(), fields)
}

/// Registra el tokenizer `es_folding` en el Index. Hay que llamarlo en cada
/// Index recién creado o abierto, porque el analizador (a diferencia de su
/// nombre, que sí queda en el esquema) vive sólo en memoria.
pub fn register_tokenizers(index: &Index) {
    let analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .filter(SpanishPluralFilter)
        .build();
    index.tokenizers().register(ES_TOKENIZER, analyzer);
}

pub async fn reindex() -> Result<()> {
    let database_url = std::env::var("DB_URL")
        .expect("Variable DB_URL no encontrada");

    let pool = PgPool::connect(database_url.as_str()).await?;

    let (schema, fields) = build_schema();

    if Path::new("./search_index").exists() {
        std::fs::remove_dir_all("./search_index")?;
    }

    std::fs::create_dir_all("./search_index")?;

    let index = Index::create_in_dir("./search_index", schema)?;
    register_tokenizers(&index);

    let mut index_writer = index.writer(50_000_000)?;

    index_noticias(&pool, &mut index_writer, fields).await?;
    index_infodocs(&pool, &mut index_writer, fields).await?;

    index_writer.commit()?;

    Ok(())
}

pub async fn index_noticias(
    pool: &PgPool,
    index_writer: &mut tantivy::IndexWriter,
    fields: SearchFields,
) -> Result<()> {
    let noticias: Vec<Noticia> = sqlx::query_as::<_, Noticia>(
        r#"
        SELECT 
            id::text AS id,
            COALESCE(titulo, '') AS titulo,
            COALESCE(subtitulo, '') AS subtitulo,
            COALESCE(extracted_text, '') AS contenido,
            fecha::text AS fecha
        FROM noticias
        "#
    )
    .fetch_all(pool)
    .await?;

    for noticia in noticias {
        let uid = make_uid("noticia", noticia.id.as_str());

        index_writer.add_document(doc!(
            fields.id => noticia.id,
            fields.uid => uid,
            fields.tipo => "noticia",
            fields.titulo => noticia.titulo,
            fields.subtitulo => noticia.subtitulo,
            fields.contenido => noticia.contenido,
            fields.fecha => noticia.fecha,
        ))?;
    }

    Ok(())
}

pub async fn index_infodocs(
    pool: &PgPool,
    index_writer: &mut tantivy::IndexWriter,
    fields: SearchFields,
) -> Result<()> {
    let info_docs: Vec<InfoDoc> = sqlx::query_as::<_, InfoDoc>(
        r#"
        SELECT 
            id::text AS id,
            COALESCE(infotitle, '') AS info_title,
            COALESCE(title, '') AS title,
            COALESCE(extracted_text, '') AS contenido,
            COALESCE(subtitle, '') AS subtitle,
            created_at::text AS created_at
        FROM infoTabs
        "#
    )
    .fetch_all(pool)
    .await?;

    for info_doc in info_docs {
        let uid = make_uid("info_doc", info_doc.id.as_str());

        index_writer.add_document(doc!(
            fields.id => info_doc.id,
            fields.uid => uid,
            fields.tipo => "info_doc",
            fields.info_title => info_doc.info_title,
            fields.titulo => info_doc.title,
            fields.subtitulo => info_doc.subtitle,
            fields.contenido => info_doc.contenido,
            fields.fecha => info_doc.created_at,
        ))?;
    }

    Ok(())
}