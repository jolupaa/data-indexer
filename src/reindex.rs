use anyhow::{Context, Result, bail};
use futures_util::TryStreamExt;
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use tantivy::{
    IndexWriter, TantivyError,
    directory::{Directory, DirectoryLock, INDEX_WRITER_LOCK, MmapDirectory},
    doc,
};

use crate::init::{SearchFields, create_index_in, explain_lock_error};
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
/// El índice nuevo se construye en un directorio aparte y sólo sustituye al
/// actual cuando está completo: si la base de datos falla a mitad, el índice
/// anterior sigue intacto. Mientras dura, se mantiene el lock de escritura del
/// índice actual, así que no puede ejecutarse con un `serve` en marcha sobre el
/// mismo directorio (el servidor seguiría escribiendo en el índice sustituido).
pub async fn reindex() -> Result<()> {
    let database_url = std::env::var("DB_URL").context("variable DB_URL no encontrada")?;
    let dir = index_dir();

    let lock = lock_existing_index(&dir)?;

    let tmp_dir = sibling_dir(&dir, "reindex-tmp")?;
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)
            .with_context(|| format!("no se pudo borrar {}", tmp_dir.display()))?;
    }
    std::fs::create_dir_all(&tmp_dir)
        .with_context(|| format!("no se pudo crear {}", tmp_dir.display()))?;

    if let Err(err) = build_index(&database_url, &tmp_dir).await {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(err);
    }

    // Soltamos el lock antes de mover directorios (en Windows no se puede
    // renombrar un directorio con ficheros abiertos).
    drop(lock);
    replace_dir(&tmp_dir, &dir)
}

async fn build_index(database_url: &str, dir: &Path) -> Result<()> {
    let pool = PgPool::connect(database_url)
        .await
        .context("no se pudo conectar a PostgreSQL")?;

    let (index, fields) = create_index_in(dir)?;
    let mut index_writer: IndexWriter = index.writer(WRITER_MEMORY_BYTES)?;

    let noticias = index_noticias(&pool, &index_writer, fields).await?;
    let info_docs = index_infodocs(&pool, &index_writer, fields).await?;

    index_writer.commit()?;
    // Deja terminadas las fusiones de segmentos antes de salir del proceso.
    index_writer.wait_merging_threads()?;
    pool.close().await;

    println!("Indexados {noticias} noticias y {info_docs} info_docs.");
    Ok(())
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

/// Si ya hay un índice en `dir`, toma su lock de escritura (falla si un
/// `serve` lo tiene abierto). Se niega a continuar si `dir` tiene contenido que
/// no es un índice, para no acabar borrando un directorio equivocado por un
/// `INDEX_DIR` mal puesto.
fn lock_existing_index(dir: &Path) -> Result<Option<DirectoryLock>> {
    if !dir.exists() {
        return Ok(None);
    }
    if !dir.is_dir() {
        bail!("{} existe pero no es un directorio", dir.display());
    }
    if !dir.join("meta.json").is_file() {
        let is_empty = std::fs::read_dir(dir)?.next().is_none();
        if is_empty {
            return Ok(None);
        }
        bail!(
            "{} no está vacío y no contiene un índice (falta meta.json); no se reemplazará",
            dir.display()
        );
    }

    let directory =
        MmapDirectory::open(dir).with_context(|| format!("no se pudo abrir {}", dir.display()))?;
    let lock = directory
        .acquire_lock(&INDEX_WRITER_LOCK)
        .map_err(|err| explain_lock_error(TantivyError::LockFailure(err, None), dir))?;
    Ok(Some(lock))
}

/// `dir` con un sufijo en el nombre: `./search_index` → `./search_index.<suffix>`.
fn sibling_dir(dir: &Path, suffix: &str) -> Result<PathBuf> {
    let name = dir
        .file_name()
        .with_context(|| format!("INDEX_DIR no válido: {}", dir.display()))?;
    let mut sibling = name.to_os_string();
    sibling.push(".");
    sibling.push(suffix);
    Ok(dir.with_file_name(sibling))
}

/// Sustituye `dir` por `new_dir`, restaurando el original si algo falla.
fn replace_dir(new_dir: &Path, dir: &Path) -> Result<()> {
    if !dir.exists() {
        return std::fs::rename(new_dir, dir)
            .with_context(|| format!("no se pudo mover el índice nuevo a {}", dir.display()));
    }

    let old_dir = sibling_dir(dir, "reindex-old")?;
    if old_dir.exists() {
        std::fs::remove_dir_all(&old_dir)
            .with_context(|| format!("no se pudo borrar {}", old_dir.display()))?;
    }
    std::fs::rename(dir, &old_dir)
        .with_context(|| format!("no se pudo apartar el índice anterior {}", dir.display()))?;

    if let Err(err) = std::fs::rename(new_dir, dir) {
        let _ = std::fs::rename(&old_dir, dir);
        return Err(err)
            .with_context(|| format!("no se pudo mover el índice nuevo a {}", dir.display()));
    }

    if let Err(err) = std::fs::remove_dir_all(&old_dir) {
        eprintln!(
            "Aviso: no se pudo borrar el índice anterior {}: {err}",
            old_dir.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::open_index;

    #[test]
    fn sibling_dir_appends_suffix() {
        assert_eq!(
            sibling_dir(Path::new("./search_index"), "reindex-tmp").unwrap(),
            PathBuf::from("./search_index.reindex-tmp")
        );
        assert_eq!(
            sibling_dir(Path::new("data/idx/"), "reindex-old").unwrap(),
            PathBuf::from("data/idx.reindex-old")
        );
        assert!(sibling_dir(Path::new("/"), "x").is_err());
        assert!(sibling_dir(Path::new("."), "x").is_err());
    }

    #[test]
    fn refuses_to_replace_a_directory_that_is_not_an_index() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("importante.txt"), "no borrar").unwrap();
        assert!(lock_existing_index(tmp.path()).is_err());

        let empty = tempfile::tempdir().unwrap();
        assert!(lock_existing_index(empty.path()).unwrap().is_none());
    }

    #[test]
    fn refuses_while_the_index_is_locked_by_a_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let (index, _) = create_index_in(tmp.path()).unwrap();
        let writer: IndexWriter = index.writer(15_000_000).unwrap();

        let Err(err) = lock_existing_index(tmp.path()) else {
            panic!("debería fallar con el writer abierto");
        };
        assert!(err.to_string().contains("en uso"), "{err}");

        drop(writer);
        assert!(lock_existing_index(tmp.path()).unwrap().is_some());
    }

    #[test]
    fn replace_dir_swaps_in_the_new_index() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("search_index");
        let new_dir = root.path().join("search_index.reindex-tmp");

        std::fs::create_dir_all(&dir).unwrap();
        create_index_in(&dir).unwrap();
        std::fs::write(dir.join("viejo"), "").unwrap();

        std::fs::create_dir_all(&new_dir).unwrap();
        create_index_in(&new_dir).unwrap();

        replace_dir(&new_dir, &dir).unwrap();

        assert!(!new_dir.exists());
        assert!(!root.path().join("search_index.reindex-old").exists());
        assert!(!dir.join("viejo").exists());
        open_index(&dir).unwrap();
    }
}
