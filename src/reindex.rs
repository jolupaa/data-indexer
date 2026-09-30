use anyhow::{Context, Result, bail};
use futures_util::TryStreamExt;
use sqlx::PgPool;
use std::path::Path;
use tantivy::{
    Index, IndexWriter, TantivyError,
    directory::{Directory, INDEX_WRITER_LOCK, MmapDirectory},
    doc,
};

use crate::init::{SearchFields, create_index_in, explain_lock_error, open_index};
use crate::utils::*;

/// Memoria del `IndexWriter` durante la carga masiva.
const WRITER_MEMORY_BYTES: usize = 50_000_000;

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

/// Reconstruye el índice completo desde PostgreSQL.
///
/// Se reconstruye dentro del propio índice: se marcan todos los documentos como
/// borrados, se añaden los de la base de datos y se confirma todo en un único
/// commit. Hasta ese commit el índice anterior sigue intacto (si PostgreSQL
/// falla a mitad no se pierde nada), y no hace falta mover ni borrar el
/// directorio, lo que fallaría si `INDEX_DIR` es un punto de montaje.
///
/// Durante todo el proceso se mantiene el lock de escritura del índice, así que
/// no puede ejecutarse con un `serve` en marcha sobre el mismo directorio.
pub async fn reindex() -> Result<()> {
    let database_url = std::env::var("DB_URL").context("variable DB_URL no encontrada")?;
    let dir = index_dir();

    let (mut index_writer, fields) = open_for_rebuild(&dir)?;

    let pool = PgPool::connect(&database_url)
        .await
        .context("no se pudo conectar a PostgreSQL")?;

    let noticias = index_noticias(&pool, &index_writer, fields).await?;
    let info_docs = index_infodocs(&pool, &index_writer, fields).await?;

    index_writer.commit()?;
    // Deja terminadas las fusiones de segmentos antes de salir del proceso.
    index_writer.wait_merging_threads()?;
    pool.close().await;

    println!("Indexados {noticias} noticias y {info_docs} info_docs.");
    Ok(())
}

/// Prepara el índice de `dir` para reconstruirlo: lo crea si no existe, toma
/// su lock de escritura y deja pendiente el borrado de todo su contenido.
fn open_for_rebuild(dir: &Path) -> Result<(IndexWriter, SearchFields)> {
    let (index, fields) = prepare_index(dir)?;
    let index_writer: IndexWriter = index
        .writer(WRITER_MEMORY_BYTES)
        .map_err(|err| explain_lock_error(err, dir))?;
    index_writer.delete_all_documents()?;
    Ok((index_writer, fields))
}

/// Abre el índice de `dir` o lo crea si no hay ninguno. Se niega a tocar un
/// directorio con contenido que no sea un índice, para no escribir en uno
/// equivocado por un `INDEX_DIR` mal puesto.
///
/// Un índice que no se puede abrir con el esquema actual (el de una versión
/// anterior, o uno dañado) no sirve para nada: se vacía y se crea de nuevo.
fn prepare_index(dir: &Path) -> Result<(Index, SearchFields)> {
    if !dir.join("meta.json").is_file() {
        if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
            bail!(
                "{} no está vacío y no contiene un índice (falta meta.json); no se usará",
                dir.display()
            );
        }
        std::fs::create_dir_all(dir)
            .with_context(|| format!("no se pudo crear {}", dir.display()))?;
        return create_index_in(dir);
    }

    match open_index(dir) {
        Ok(opened) => Ok(opened),
        Err(_) => {
            println!(
                "El índice de {} es de otra versión o está dañado: se crea de nuevo.",
                dir.display()
            );
            recreate_index(dir)
        }
    }
}

/// Borra el índice de `dir` y crea uno vacío con el esquema actual, con el lock
/// de escritura tomado para no pisar a un `serve` (de esta u otra versión).
fn recreate_index(dir: &Path) -> Result<(Index, SearchFields)> {
    let directory =
        MmapDirectory::open(dir).with_context(|| format!("no se pudo abrir {}", dir.display()))?;
    let _lock = directory
        .acquire_lock(&INDEX_WRITER_LOCK)
        .map_err(|err| explain_lock_error(TantivyError::LockFailure(err, None), dir))?;

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() == INDEX_WRITER_LOCK.filepath.as_os_str() {
            continue;
        }
        let path = entry.path();
        let removed = if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.with_context(|| format!("no se pudo borrar {}", path.display()))?;
    }

    create_index_in(dir)
}

pub async fn index_noticias(
    pool: &PgPool,
    index_writer: &IndexWriter,
    fields: SearchFields,
) -> Result<usize> {
    let mut rows = sqlx::query_as::<_, Noticia>(
        r#"
        SELECT
            id::text AS id,
            COALESCE(titulo, '') AS titulo,
            COALESCE(subtitulo, '') AS subtitulo,
            COALESCE(extracted_text, '') AS contenido,
            COALESCE(fecha::text, '') AS fecha
        FROM noticias
        "#,
    )
    .fetch(pool);

    let mut count = 0;
    while let Some(noticia) = rows.try_next().await.context("error leyendo noticias")? {
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
        count += 1;
    }

    Ok(count)
}

pub async fn index_infodocs(
    pool: &PgPool,
    index_writer: &IndexWriter,
    fields: SearchFields,
) -> Result<usize> {
    let mut rows = sqlx::query_as::<_, InfoDoc>(
        r#"
        SELECT
            id::text AS id,
            COALESCE(infotitle, '') AS info_title,
            COALESCE(title, '') AS title,
            COALESCE(extracted_text, '') AS contenido,
            COALESCE(subtitle, '') AS subtitle,
            COALESCE(created_at::text, '') AS created_at
        FROM infoTabs
        "#,
    )
    .fetch(pool);

    let mut count = 0;
    while let Some(info_doc) = rows.try_next().await.context("error leyendo infoTabs")? {
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
        count += 1;
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::{
        Term, collector::Count, query::AllQuery, schema::STORED, schema::STRING, schema::Schema,
        schema::TEXT,
    };

    fn num_docs(dir: &Path) -> usize {
        let (index, _) = open_index(dir).unwrap();
        let reader = index.reader().unwrap();
        reader.searcher().search(&AllQuery, &Count).unwrap()
    }

    fn add_one(dir: &Path, id: &str) {
        let (index, fields) = open_index(dir).unwrap();
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        writer.add_document(doc!(fields.id => id)).unwrap();
        writer.commit().unwrap();
    }

    #[test]
    fn creates_the_index_when_missing() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("nuevo").join("search_index");
        open_for_rebuild(&dir).unwrap();
        open_index(&dir).unwrap();
    }

    #[test]
    fn refuses_a_non_empty_directory_that_is_not_an_index() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("importante.txt");
        std::fs::write(&file, "no tocar").unwrap();
        assert!(open_for_rebuild(tmp.path()).is_err());
        assert!(file.exists());
    }

    #[test]
    fn refuses_while_the_index_is_locked_by_a_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let (index, _) = create_index_in(tmp.path()).unwrap();
        let writer: IndexWriter = index.writer(15_000_000).unwrap();

        let Err(err) = open_for_rebuild(tmp.path()) else {
            panic!("debería fallar con el writer abierto");
        };
        assert!(err.to_string().contains("en uso"), "{err}");

        drop(writer);
        open_for_rebuild(tmp.path()).unwrap();
    }

    #[test]
    fn old_documents_survive_until_the_rebuild_commits() {
        let tmp = tempfile::tempdir().unwrap();
        create_index_in(tmp.path()).unwrap();
        add_one(tmp.path(), "viejo");

        // Una reconstrucción que falla antes del commit no borra nada.
        let (writer, fields) = open_for_rebuild(tmp.path()).unwrap();
        writer.add_document(doc!(fields.id => "a medias")).unwrap();
        drop(writer);
        assert_eq!(num_docs(tmp.path()), 1);

        // Una que llega al commit sustituye todo el contenido.
        let (mut writer, fields) = open_for_rebuild(tmp.path()).unwrap();
        writer.add_document(doc!(fields.id => "nuevo")).unwrap();
        writer.commit().unwrap();
        drop(writer);

        let (index, fields) = open_index(tmp.path()).unwrap();
        let searcher = index.reader().unwrap().searcher();
        assert_eq!(searcher.search(&AllQuery, &Count).unwrap(), 1);
        let nuevo = tantivy::query::TermQuery::new(
            Term::from_field_text(fields.id, "nuevo"),
            tantivy::schema::IndexRecordOption::Basic,
        );
        assert_eq!(searcher.search(&nuevo, &Count).unwrap(), 1);
    }

    #[test]
    fn recreates_an_index_from_an_older_version() {
        let tmp = tempfile::tempdir().unwrap();
        let mut builder = Schema::builder();
        builder.add_text_field("id", STRING | STORED);
        builder.add_text_field("titulo", TEXT | STORED);
        Index::create_in_dir(tmp.path(), builder.build()).unwrap();

        open_for_rebuild(tmp.path()).unwrap();
        open_index(tmp.path()).unwrap();
    }
}
