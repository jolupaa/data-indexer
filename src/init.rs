use anyhow::Result;
use sqlx::PgPool;
use std::path::Path;
use tantivy::{
    doc,
    schema::{Field, Schema, STRING, STORED, TEXT},
    Index,
};

use crate::lib::*;


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

pub async fn reindex() -> Result<()> {
    let database_url = std::env::var("DB_URL")
        .expect("Variable DB_URL no encontrada");

    let pool = PgPool::connect(database_url.as_str()).await?;

    let mut schema_builder = Schema::builder();

    let fields = SearchFields {
        id: schema_builder.add_text_field("id", STRING | STORED),
        uid: schema_builder.add_text_field("uid", STRING | STORED),
        tipo: schema_builder.add_text_field("tipo", STRING | STORED),
        info_title: schema_builder.add_text_field("info_title", TEXT | STORED),
        titulo: schema_builder.add_text_field("titulo", TEXT | STORED),
        subtitulo: schema_builder.add_text_field("subtitulo", TEXT | STORED),
        contenido: schema_builder.add_text_field("contenido", TEXT),
        fecha: schema_builder.add_text_field("fecha", STRING | STORED),
    };

    let schema = schema_builder.build();

    if Path::new("./search_index").exists() {
        std::fs::remove_dir_all("./search_index")?;
    }

    std::fs::create_dir_all("./search_index")?;

    let index = Index::create_in_dir("./search_index", schema)?;

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