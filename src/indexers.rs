use anyhow::Result;
use sqlx::PgPool;
use tantivy::{
    doc
};
use tantivy::{
    schema::{Schema, TEXT, STRING, STORED, Field},
    Index,
};

#[derive(sqlx::FromRow)]
struct Noticia {
    id: String,
    titulo: String,
    contenido: String,
    autor: Option<String>,
    fecha: Option<String>,
}


#[derive(sqlx::FromRow)]
struct InfoDoc {
    id: String,
    titulo: String,
    contenido: String
}

#[derive(Clone, Copy)]
pub struct SearchFields {
    id: Field,
    tipo: Field,
    titulo: Field,
    contenido: Field,
    autor: Field,
    fecha: Field,
}

pub async fn start_indexing() -> Result<(), anyhow::Error> {
    let database_url = std::env::var("DB_URL").expect("Variable DB_URL no encontrada");;

    let pool = PgPool::connect(database_url.as_str()).await?;

    let mut schema_builder = Schema::builder();

    let fields = SearchFields {
        id: schema_builder.add_text_field("id", STRING | STORED),
        tipo: schema_builder.add_text_field("tipo", STRING | STORED),
        titulo: schema_builder.add_text_field("titulo", TEXT | STORED),
        contenido: schema_builder.add_text_field("contenido", TEXT),
        autor: schema_builder.add_text_field("autor", TEXT | STORED),
        fecha: schema_builder.add_text_field("fecha", STRING | STORED),
    };

    let schema = schema_builder.build();

    let index = Index::create_in_dir("./search_index", schema)?;

    let mut index_writer = index.writer(50_000_000)?;


    //all indexers needed
    index_noticias(&pool, &mut index_writer, fields).await?;
    index_infodocs(&pool, &mut index_writer, fields).await?;


    index_writer.commit()?;

    Ok(())
}


pub async fn index_infodocs(
    pool: &PgPool,
    index_writer: &mut tantivy::IndexWriter,
    fields: SearchFields,
) -> Result<()> {
    let info_docs: Vec<InfoDoc> = sqlx::query_as::<_, InfoDoc>(
        r#"
        SELECT id, titulo, contenido, autor, fecha
        FROM noticias
        "#
    )
    .fetch_all(pool)
    .await?;

    for info_doc in info_docs {
        index_writer.add_document(doc!(
            fields.id => info_doc.id,
            fields.tipo => "info_doc",
            fields.titulo => info_doc.titulo,
            fields.contenido => info_doc.contenido
        ))?;
    }

    Ok(())
}

pub async fn index_noticias(
    pool: &PgPool,
    index_writer: &mut tantivy::IndexWriter,
    fields: SearchFields,
) -> Result<()> {
    let noticias: Vec<Noticia> = sqlx::query_as::<_, Noticia>(
        r#"
        SELECT id, titulo, contenido
        FROM infoTabs
        "#
    )
    .fetch_all(pool)
    .await?;

    for noticia in noticias {
        index_writer.add_document(doc!(
            fields.id => noticia.id,
            fields.tipo => "InfoDoc",
            fields.titulo => noticia.titulo,
            fields.contenido => noticia.contenido,
            fields.autor => noticia.autor.unwrap_or_default(),
            fields.fecha => noticia.fecha.unwrap_or_default(),
        ))?;
    }

    Ok(())
}