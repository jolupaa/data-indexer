use anyhow::{Context, Result, bail};
use futures_util::TryStreamExt;
use sqlx::PgPool;
use std::path::Path;
use tantivy::{
    Index, IndexWriter, TantivyError,
    directory::{Directory, INDEX_WRITER_LOCK, MmapDirectory},
};

use crate::indexer::{IndexDocumentRequest, document_uid, into_document};
use crate::init::{
    IndexStatus, REBUILD_MARKER, SearchFields, WRITER_MEMORY_BYTES, create_index_in,
    explain_lock_error, inspect_index,
};
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

/// Reconstruye el índice completo desde PostgreSQL.
///
/// Se reconstruye dentro del propio índice: se marcan todos los documentos como
/// borrados, se añaden los de la base de datos y se confirma todo en un único
/// commit. Hasta ese commit el índice en disco conserva su contenido anterior
/// (si PostgreSQL falla a mitad no se pierde nada), y no hace falta mover ni
/// borrar el directorio, lo que fallaría si `INDEX_DIR` es un punto de montaje.
///
/// Durante todo el proceso se mantiene el lock de escritura del índice, así que
/// no puede ejecutarse con un `serve` en marcha sobre el mismo directorio.
pub async fn reindex() -> Result<()> {
    let database_url = std::env::var("DB_URL").context("variable DB_URL no encontrada")?;
    let dir = index_dir();

    // Primero la conexión: si la base de datos no responde, no se toca nada.
    let pool = PgPool::connect(&database_url)
        .await
        .context("no se pudo conectar a PostgreSQL")?;

    let (index_writer, fields) = open_for_rebuild(&dir)?;

    let noticias = index_noticias(&pool, &index_writer, fields).await?;
    let info_docs = index_infodocs(&pool, &index_writer, fields).await?;

    finish_rebuild(&dir, index_writer)?;
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

/// Confirma la reconstrucción y da por terminada la de un índice creado desde
/// cero (ver `REBUILD_MARKER`).
fn finish_rebuild(dir: &Path, mut index_writer: IndexWriter) -> Result<()> {
    index_writer.commit()?;

    // Con el commit hecho, el índice ya está completo aunque lo que sigue
    // (esperar a las fusiones) falle o se interrumpa.
    match std::fs::remove_file(dir.join(REBUILD_MARKER)) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            return Err(err).context("no se pudo borrar la marca de reconstrucción incompleta");
        }
        _ => {}
    }

    // Deja terminadas las fusiones de segmentos antes de salir del proceso.
    index_writer.wait_merging_threads()?;
    Ok(())
}

/// Abre el índice de `dir` o lo crea si no hay ninguno. Un índice de una versión
/// anterior no sirve para esta: se sustituye por uno vacío. Cualquier otro error
/// al abrirlo se devuelve sin tocar nada.
fn prepare_index(dir: &Path) -> Result<(Index, SearchFields)> {
    match inspect_index(dir)? {
        IndexStatus::Ready(index, fields) => Ok((index, fields)),
        IndexStatus::Missing => {
            ensure_only_index_files(dir)?;
            std::fs::create_dir_all(dir)
                .with_context(|| format!("no se pudo crear {}", dir.display()))?;
            recreate_index(dir)
        }
        IndexStatus::Outdated => {
            println!(
                "El índice de {} es de una versión anterior: se crea de nuevo.",
                dir.display()
            );
            recreate_index(dir)
        }
    }
}

/// Se niega a usar un directorio con contenido ajeno al índice, para no
/// mezclarlo con datos de otra cosa por un `INDEX_DIR` mal puesto.
fn ensure_only_index_files(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if !is_index_file(&name) && name != "lost+found" {
            bail!(
                "{} contiene ficheros que no son de un índice (p. ej. {name}); no se usará",
                dir.display()
            );
        }
    }
    Ok(())
}

/// Ficheros que crea tantivy (o `reindex`) en el directorio del índice.
fn is_index_file(name: &str) -> bool {
    is_segment_file(name)
        || name == "meta.json"
        || name == ".managed.json"
        || name.starts_with(".tantivy-")
        // Temporales de las escrituras atómicas de tantivy (p. ej. de meta.json)
        // que quedan si el proceso muere a mitad.
        || name.starts_with(".tmp")
        || name == REBUILD_MARKER
}

/// "<uuid en 32 hex>.<componente>", con los componentes que usa tantivy. Mirar
/// sólo el uuid confundiría con segmentos cualquier fichero nombrado por su
/// hash MD5 (p. ej. "d41d8cd98f00b204e9800998ecf8427e.jpg").
fn is_segment_file(name: &str) -> bool {
    let Some((uuid, component)) = name.split_once('.') else {
        return false;
    };
    let is_opstamp = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    uuid.len() == 32
        && uuid.bytes().all(|b| b.is_ascii_hexdigit())
        && (matches!(
            component,
            "idx" | "pos" | "term" | "store" | "store.temp" | "fast" | "fieldnorm"
        ) || component.strip_suffix(".del").is_some_and(is_opstamp))
}

/// Borra el índice de `dir` (sólo sus ficheros: `lost+found` o cualquier otra
/// cosa se deja en paz) y crea uno vacío con el esquema actual. Lo hace con el
/// lock de escritura tomado, para no pisar a un `serve` de esta u otra versión,
/// y dejando la marca de reconstrucción incompleta hasta que `reindex` confirme
/// la carga: así `serve` no sirve un índice vacío si la carga falla.
fn recreate_index(dir: &Path) -> Result<(Index, SearchFields)> {
    let directory =
        MmapDirectory::open(dir).with_context(|| format!("no se pudo abrir {}", dir.display()))?;
    let _lock = directory
        .acquire_lock(&INDEX_WRITER_LOCK)
        .map_err(|err| explain_lock_error(TantivyError::LockFailure(err, None), dir))?;

    std::fs::write(dir.join(REBUILD_MARKER), b"")
        .context("no se pudo crear la marca de reconstrucción incompleta")?;

    let mut stale: Vec<_> = std::fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()?;
    stale.retain(|path| {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        is_index_file(&name) && !name.starts_with(".tantivy-") && name != REBUILD_MARKER
    });
    // `meta.json` el último: si algo falla antes, sigue siendo un índice
    // (viejo) que el siguiente `reindex` reconocerá y volverá a sustituir.
    stale.sort_by_key(|path| path.ends_with("meta.json"));
    for path in stale {
        std::fs::remove_file(&path)
            .with_context(|| format!("no se pudo borrar {}", path.display()))?;
    }

    create_index_in(dir)
}

/// Añade una fila al índice con las mismas funciones que usa `/index/upsert`,
/// para que el documento tenga la misma forma venga por donde venga. Las filas
/// que no pasan la validación se omiten con un aviso.
fn index_row(
    index_writer: &IndexWriter,
    fields: SearchFields,
    request: IndexDocumentRequest,
) -> Result<bool> {
    match document_uid(&request.tipo, &request.id) {
        Ok(uid) => {
            let (_, document) = into_document(&fields, request, uid);
            index_writer.add_document(document)?;
            Ok(true)
        }
        Err(err) => {
            eprintln!(
                "Aviso: se omite {}:{}: {err}",
                preview(&request.tipo),
                preview(&request.id)
            );
            Ok(false)
        }
    }
}

/// Los primeros caracteres de `text`, para no volcar al log un id de megas.
fn preview(text: &str) -> String {
    const MAX_CHARS: usize = 80;
    match text.char_indices().nth(MAX_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
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
        let request = IndexDocumentRequest {
            id: noticia.id,
            tipo: "noticia".to_string(),
            titulo: noticia.titulo,
            subtitulo: noticia.subtitulo,
            contenido: noticia.contenido,
            fecha: noticia.fecha,
            info_title: String::new(),
        };
        if index_row(index_writer, fields, request)? {
            count += 1;
        }
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
        let request = IndexDocumentRequest {
            id: info_doc.id,
            tipo: "info_doc".to_string(),
            titulo: info_doc.title,
            subtitulo: info_doc.subtitle,
            contenido: info_doc.contenido,
            fecha: info_doc.created_at,
            info_title: info_doc.info_title,
        };
        if index_row(index_writer, fields, request)? {
            count += 1;
        }
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::open_index;
    use tantivy::{
        Term, collector::Count, doc, query::AllQuery, schema::STORED, schema::Schema, schema::TEXT,
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
        let (writer, _) = open_for_rebuild(&dir).unwrap();

        // Mientras no se confirme la carga, `serve` no lo da por bueno.
        drop(writer);
        let Err(err) = open_index(&dir) else {
            panic!("no debería servir un índice a medio construir");
        };
        assert!(err.to_string().contains("no terminó"), "{err}");

        let (writer, _) = open_for_rebuild(&dir).unwrap();
        finish_rebuild(&dir, writer).unwrap();
        open_index(&dir).unwrap();
    }

    #[test]
    fn accepts_a_fresh_mount_point_with_lost_and_found() {
        let tmp = tempfile::tempdir().unwrap();
        let lost_found = tmp.path().join("lost+found");
        std::fs::create_dir(&lost_found).unwrap();

        let (writer, _) = open_for_rebuild(tmp.path()).unwrap();
        finish_rebuild(tmp.path(), writer).unwrap();
        open_index(tmp.path()).unwrap();
        assert!(lost_found.is_dir());
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
        let (writer, fields) = open_for_rebuild(tmp.path()).unwrap();
        writer.add_document(doc!(fields.id => "nuevo")).unwrap();
        finish_rebuild(tmp.path(), writer).unwrap();

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
    fn does_not_wipe_an_index_it_cannot_open() {
        let tmp = tempfile::tempdir().unwrap();
        create_index_in(tmp.path()).unwrap();
        add_one(tmp.path(), "valioso");
        let meta = tmp.path().join("meta.json");
        let original = std::fs::read(&meta).unwrap();
        std::fs::write(&meta, b"{ no es json").unwrap();
        let files_before = std::fs::read_dir(tmp.path()).unwrap().count();

        assert!(open_for_rebuild(tmp.path()).is_err());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), files_before);

        std::fs::write(&meta, original).unwrap();
        assert_eq!(num_docs(tmp.path()), 1);
    }

    #[test]
    fn recreates_an_index_from_an_older_version() {
        let tmp = tempfile::tempdir().unwrap();
        let schema = crate::init::tests::previous_version_schema();
        let id = schema.get_field("id").unwrap();
        let old = Index::create_in_dir(tmp.path(), schema).unwrap();
        old.tokenizers()
            .register("es_folding", crate::init::es_analyzer());
        let mut old_writer: IndexWriter = old.writer(15_000_000).unwrap();
        old_writer.add_document(doc!(id => "viejo")).unwrap();
        old_writer.commit().unwrap();
        drop(old_writer);

        let notes = tmp.path().join("notas.txt");
        std::fs::write(&notes, "del usuario").unwrap();
        std::fs::create_dir(tmp.path().join("lost+found")).unwrap();

        // Si la carga falla tras recrearlo, `serve` no sirve el índice vacío.
        let (writer, _) = open_for_rebuild(tmp.path()).unwrap();
        drop(writer);
        assert!(open_index(tmp.path()).is_err());

        let (writer, _) = open_for_rebuild(tmp.path()).unwrap();
        finish_rebuild(tmp.path(), writer).unwrap();
        assert_eq!(num_docs(tmp.path()), 0);

        // Sólo se borran los ficheros del índice.
        assert!(notes.exists());
        assert!(tmp.path().join("lost+found").is_dir());
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.len() > 33 && name.as_bytes()[32] == b'.')
            .collect();
        let (index, _) = open_index(tmp.path()).unwrap();
        let live: Vec<String> = index
            .searchable_segment_metas()
            .unwrap()
            .iter()
            .flat_map(|meta| meta.list_files())
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        assert!(
            leftovers.iter().all(|name| live.contains(name)),
            "quedan segmentos del índice viejo: {leftovers:?}"
        );
    }

    #[test]
    fn recognises_index_files() {
        assert!(is_index_file("meta.json"));
        assert!(is_index_file(".managed.json"));
        assert!(is_index_file(".tantivy-writer.lock"));
        assert!(is_index_file(".tmpAbC123"));
        assert!(is_index_file("0123456789abcdef0123456789abcdef.idx"));
        assert!(is_index_file("0123456789abcdef0123456789abcdef.store.temp"));
        assert!(is_index_file("0123456789abcdef0123456789abcdef.12.del"));
        assert!(!is_index_file("lost+found"));
        assert!(!is_index_file("notas.txt"));
        assert!(!is_index_file("0123456789abcdef0123456789abcdeg.idx"));
        // Ficheros nombrados por su hash MD5: no son nuestros.
        assert!(!is_index_file("d41d8cd98f00b204e9800998ecf8427e.jpg"));
        assert!(!is_index_file("d41d8cd98f00b204e9800998ecf8427e.del"));
        assert!(!is_index_file("d41d8cd98f00b204e9800998ecf8427e.x.del"));
    }

    #[test]
    fn refuses_a_tantivy_index_from_another_application() {
        let tmp = tempfile::tempdir().unwrap();
        let mut builder = Schema::builder();
        builder.add_text_field("body", TEXT | STORED);
        Index::create_in_dir(tmp.path(), builder.build()).unwrap();
        let files_before = std::fs::read_dir(tmp.path()).unwrap().count();

        let Err(err) = open_for_rebuild(tmp.path()) else {
            panic!("no debería tocar el índice de otra aplicación");
        };
        assert!(err.to_string().contains("no es de data-indexer"), "{err}");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), files_before);
    }

    #[test]
    fn previews_long_values() {
        assert_eq!(preview("123"), "123");
        let long = "é".repeat(1000);
        assert_eq!(preview(&long).chars().count(), 81);
    }
}
