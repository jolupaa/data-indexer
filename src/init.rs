use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::Path;
use tantivy::{
    Index, TantivyError,
    directory::error::LockError,
    schema::{Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions},
    tokenizer::{AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer},
};

use crate::nfc::NfcTokenizer;
use crate::spanish_plural::SpanishPluralFilter;

/// Nombre del tokenizer que pliega acentos (á→a, ñ→n, …) además de pasar a
/// minúsculas. Se aplica a los campos de texto buscables para que "actualizacion"
/// encuentre "actualización". Debe registrarse en cada Index que se abra (tanto
/// al reindexar como al servir) con `register_tokenizers`.
///
/// El nombre queda guardado en el esquema del índice, así que lleva versión:
/// si cambia el análisis del texto hay que subirla, y `open_index` rechazará
/// los índices creados con el analizador anterior (sus términos ya no
/// coincidirían con los de las consultas) hasta que se ejecute `reindex`.
pub const ES_TOKENIZER: &str = "es_folding_v2";

/// Memoria del `IndexWriter`, tanto en `serve` como en `reindex`.
pub const WRITER_MEMORY_BYTES: usize = 50_000_000;

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
    /// Principales que pueden ver el documento (`public`, `u:<id>`, `a:<id>`).
    /// Indexado y no guardado: nunca sale en los resultados.
    pub acl: Field,
    /// Conversación a la que pertenece un `chat_msg`. Guardado, para que el
    /// backend agrupe por conversación, e indexado, para borrarla entera.
    pub thread: Field,
}

impl SearchFields {
    /// Campos en los que busca el texto libre de `/search`.
    pub fn full_text(&self) -> Vec<Field> {
        vec![self.titulo, self.subtitulo, self.info_title, self.contenido]
    }
}

/// Opciones de un campo de texto buscable con el tokenizer que pliega acentos.
/// `stored` controla si el valor original se devuelve en los resultados.
fn folded_text(stored: bool) -> TextOptions {
    let indexing = TextFieldIndexing::default()
        .set_tokenizer(ES_TOKENIZER)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let opts = TextOptions::default().set_indexing_options(indexing);
    if stored { opts.set_stored() } else { opts }
}

/// Construye el esquema del índice y devuelve los handles de cada campo.
/// Es la única fuente de verdad del esquema: la usan tanto `reindex` (al crear
/// el índice) como el servidor (que comprueba que el índice en disco coincide).
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
        // v3: siempre al final, para que los campos anteriores no cambien.
        acl: schema_builder.add_text_field("acl", STRING),
        thread: schema_builder.add_text_field("thread", STRING | STORED),
    };

    (schema_builder.build(), fields)
}

/// Registra el tokenizer `ES_TOKENIZER` en el Index. Hay que llamarlo en cada
/// Index recién creado o abierto, porque el analizador (a diferencia de su
/// nombre, que sí queda en el esquema) vive sólo en memoria.
pub fn register_tokenizers(index: &Index) {
    index.tokenizers().register(ES_TOKENIZER, es_analyzer());
}

/// El analizador de `ES_TOKENIZER`. Si cambia lo que produce, hay que subir la
/// versión de `ES_TOKENIZER`.
pub fn es_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(NfcTokenizer::new(SimpleTokenizer::default()))
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .filter(SpanishPluralFilter)
        .build()
}

/// Crea un índice vacío con el esquema actual y el analizador registrado.
pub fn create_index_in(dir: &Path) -> Result<(Index, SearchFields)> {
    let (schema, fields) = build_schema();
    let index = Index::create_in_dir(dir, schema)
        .with_context(|| format!("no se pudo crear el índice en {}", dir.display()))?;
    register_tokenizers(&index);
    Ok((index, fields))
}

/// Fichero que `reindex` deja en el directorio mientras crea un índice desde
/// cero y que borra al confirmar la carga. Si sigue ahí, esa reconstrucción no
/// terminó y el índice está vacío o a medias.
pub const REBUILD_MARKER: &str = ".reindex-incompleto";

/// Qué hay en el directorio del índice.
pub enum IndexStatus {
    /// No hay índice (falta `meta.json`).
    Missing,
    /// Un índice creado con otro esquema o analizador (una versión anterior).
    Outdated,
    /// Un índice de esta versión, con el analizador ya registrado.
    Ready(Index, SearchFields),
}

/// Campos de los índices de cada versión de data-indexer: la primera, la que
/// añadió `uid`, `subtitulo` e `info_title` (v2) y la que añadió `acl` y
/// `thread` (v3, la actual). Un índice con otros campos es de otra aplicación
/// (o de una versión más nueva) y no se toca.
///
/// Si cambian los campos de `build_schema`, añade el conjunto nuevo al final;
/// no edites los anteriores o dejarán de reconocerse (y de migrarse) los
/// índices de esas versiones.
const KNOWN_FIELD_SETS: [&[&str]; 3] = [
    &["id", "tipo", "titulo", "contenido", "autor", "fecha"],
    &[
        "id",
        "uid",
        "tipo",
        "info_title",
        "titulo",
        "subtitulo",
        "contenido",
        "fecha",
    ],
    &[
        "id",
        "uid",
        "tipo",
        "info_title",
        "titulo",
        "subtitulo",
        "contenido",
        "fecha",
        "acl",
        "thread",
    ],
];

/// Examina el índice de `dir`. Los `Field` se reconstruyen con `build_schema`,
/// así que un índice con otro esquema haría que cada valor acabase en el campo
/// equivocado: por eso se distingue como `Outdated`.
pub fn inspect_index(dir: &Path) -> Result<IndexStatus> {
    let meta_path = dir.join("meta.json");
    if !meta_path.is_file() {
        return Ok(IndexStatus::Missing);
    }

    // Antes de abrirlo con tantivy (que puede fallar con índices de otras
    // versiones de tantivy) se mira de quién es, leyendo su esquema a mano.
    let damaged = || {
        format!(
            "no se pudo abrir el índice en {} (si es de data-indexer y está dañado, \
             vacía el directorio y ejecuta `reindex`)",
            dir.display()
        )
    };
    let meta: serde_json::Value = std::fs::read(&meta_path)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| Ok(serde_json::from_slice(&bytes)?))
        .with_context(damaged)?;
    let field_names: Vec<&str> = meta["schema"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|field| field["name"].as_str())
        .collect();
    if !KNOWN_FIELD_SETS
        .iter()
        .any(|known| same_fields(known, &field_names))
    {
        bail!(
            "{} contiene un índice que no es de data-indexer (o es de una versión \
             más nueva); no se usará",
            dir.display()
        );
    }

    let index = Index::open_in_dir(dir).with_context(damaged)?;
    let (schema, fields) = build_schema();
    if index.schema() != schema {
        return Ok(IndexStatus::Outdated);
    }

    register_tokenizers(&index);
    Ok(IndexStatus::Ready(index, fields))
}

/// Si dos listas tienen los mismos campos, sin importar el orden.
fn same_fields(a: &[&str], b: &[&str]) -> bool {
    a.len() == b.len() && a.iter().collect::<BTreeSet<_>>() == b.iter().collect::<BTreeSet<_>>()
}

/// Abre el índice de `dir` para servirlo: tiene que ser de esta versión y no
/// estar a medio reconstruir.
pub fn open_index(dir: &Path) -> Result<(Index, SearchFields)> {
    if dir.join(REBUILD_MARKER).exists() {
        bail!(
            "un `reindex` anterior no terminó y el índice de {} está incompleto; \
             ejecuta `reindex` de nuevo",
            dir.display()
        );
    }

    match inspect_index(dir)? {
        IndexStatus::Ready(index, fields) => Ok((index, fields)),
        IndexStatus::Missing => bail!(
            "no hay ningún índice en {}; ejecuta `reindex` primero",
            dir.display()
        ),
        IndexStatus::Outdated => bail!(
            "el índice en {} se creó con un esquema o analizador distinto al de esta \
             versión; ejecuta `reindex` para regenerarlo",
            dir.display()
        ),
    }
}

/// Traduce el error de "lock ocupado" de tantivy a un mensaje accionable.
pub fn explain_lock_error(err: TantivyError, dir: &Path) -> anyhow::Error {
    match err {
        TantivyError::LockFailure(LockError::LockBusy, _) => anyhow::anyhow!(
            "el índice en {} está en uso por otro proceso (¿un `serve` o `reindex` en marcha?)",
            dir.display()
        ),
        other => anyhow::Error::new(other),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tantivy::schema::TEXT;

    #[test]
    fn indexes_from_the_first_release_count_as_outdated() {
        // La primera versión no tenía `uid` ni `subtitulo`, pero sí es nuestra.
        let tmp = tempfile::tempdir().unwrap();
        let mut builder = Schema::builder();
        builder.add_text_field("id", STRING | STORED);
        builder.add_text_field("tipo", STRING | STORED);
        builder.add_text_field("titulo", TEXT | STORED);
        builder.add_text_field("contenido", TEXT);
        builder.add_text_field("autor", TEXT | STORED);
        builder.add_text_field("fecha", STRING | STORED);
        Index::create_in_dir(tmp.path(), builder.build()).unwrap();

        assert!(matches!(
            inspect_index(tmp.path()).unwrap(),
            IndexStatus::Outdated
        ));
    }

    #[test]
    fn open_index_rejects_indexes_of_other_applications() {
        // También uno que comparte nombres de campo tan corrientes como estos.
        for names in [&["body"][..], &["id", "tipo", "titulo", "cuerpo"]] {
            let tmp = tempfile::tempdir().unwrap();
            let mut builder = Schema::builder();
            for name in names {
                builder.add_text_field(name, TEXT | STORED);
            }
            Index::create_in_dir(tmp.path(), builder.build()).unwrap();

            let Err(err) = open_index(tmp.path()) else {
                panic!("no debería abrir el índice de otra aplicación: {names:?}");
            };
            assert!(err.to_string().contains("no es de data-indexer"), "{err}");
        }
    }

    #[test]
    fn the_current_schema_has_the_latest_known_fields() {
        // Si se añade un campo, hay que añadir el conjunto actual a
        // `KNOWN_FIELD_SETS` para que la siguiente versión lo reconozca.
        let (schema, _) = build_schema();
        let current: Vec<&str> = schema.fields().map(|(_, entry)| entry.name()).collect();
        let latest = KNOWN_FIELD_SETS[KNOWN_FIELD_SETS.len() - 1];
        assert!(same_fields(&current, latest), "{current:?} != {latest:?}");
    }

    #[test]
    fn open_index_requires_an_existing_index() {
        let tmp = tempfile::tempdir().unwrap();
        let Err(err) = open_index(tmp.path()) else {
            panic!("no debería abrir un directorio vacío");
        };
        assert!(err.to_string().contains("reindex"), "{err}");
    }

    /// El esquema de la primera versión: mismos campos, pero con el tokenizer
    /// `es_folding`, anterior a la corrección de plurales y de NFC.
    pub fn previous_version_schema() -> Schema {
        let text = |stored: bool| {
            let indexing = TextFieldIndexing::default()
                .set_tokenizer("es_folding")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions);
            let opts = TextOptions::default().set_indexing_options(indexing);
            if stored { opts.set_stored() } else { opts }
        };
        let mut builder = Schema::builder();
        builder.add_text_field("id", STRING | STORED);
        builder.add_text_field("uid", STRING | STORED);
        builder.add_text_field("tipo", STRING | STORED);
        builder.add_text_field("info_title", text(true));
        builder.add_text_field("titulo", text(true));
        builder.add_text_field("subtitulo", text(true));
        builder.add_text_field("contenido", text(false));
        builder.add_text_field("fecha", STRING | STORED);
        builder.build()
    }

    #[test]
    fn open_index_rejects_indexes_built_with_another_schema() {
        // Un índice de una versión anterior (otro analizador en los campos de
        // texto) tendría términos que ya no coinciden con los de las consultas.
        let tmp = tempfile::tempdir().unwrap();
        Index::create_in_dir(tmp.path(), previous_version_schema()).unwrap();

        let Err(err) = open_index(tmp.path()) else {
            panic!("no debería abrir un índice con otro esquema");
        };
        assert!(err.to_string().contains("reindex"), "{err}");
    }

    /// El esquema v2: los 8 campos de antes de `acl` y `thread`, con el
    /// analizador actual.
    pub fn v2_schema() -> Schema {
        let mut builder = Schema::builder();
        builder.add_text_field("id", STRING | STORED);
        builder.add_text_field("uid", STRING | STORED);
        builder.add_text_field("tipo", STRING | STORED);
        builder.add_text_field("info_title", folded_text(true));
        builder.add_text_field("titulo", folded_text(true));
        builder.add_text_field("subtitulo", folded_text(true));
        builder.add_text_field("contenido", folded_text(false));
        builder.add_text_field("fecha", STRING | STORED);
        builder.build()
    }

    #[test]
    fn a_v2_index_on_disk_is_outdated() {
        let tmp = tempfile::tempdir().unwrap();
        Index::create_in_dir(tmp.path(), v2_schema()).unwrap();

        assert!(matches!(
            inspect_index(tmp.path()).unwrap(),
            IndexStatus::Outdated
        ));
        let Err(err) = open_index(tmp.path()) else {
            panic!("`serve` no debería abrir un índice v2");
        };
        assert!(err.to_string().contains("ejecuta `reindex`"), "{err}");
    }

    #[test]
    fn v3_appends_acl_and_thread_after_the_v2_fields() {
        let (schema, fields) = build_schema();
        let names: Vec<&str> = schema.fields().map(|(_, entry)| entry.name()).collect();
        assert_eq!(
            names,
            [
                "id",
                "uid",
                "tipo",
                "info_title",
                "titulo",
                "subtitulo",
                "contenido",
                "fecha",
                "acl",
                "thread"
            ]
        );
        // Los campos de v2 no cambian ni de posición ni de opciones.
        let v2 = v2_schema();
        for (field, entry) in v2.fields() {
            assert_eq!(schema.get_field_entry(field), entry);
        }

        let acl = schema.get_field_entry(fields.acl);
        assert!(acl.is_indexed() && !acl.is_stored());
        let thread = schema.get_field_entry(fields.thread);
        assert!(thread.is_indexed() && thread.is_stored());
    }

    #[test]
    fn open_index_accepts_indexes_built_by_this_version() {
        let tmp = tempfile::tempdir().unwrap();
        create_index_in(tmp.path()).unwrap();
        open_index(tmp.path()).unwrap();
    }
}
