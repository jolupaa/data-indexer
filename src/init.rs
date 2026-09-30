use anyhow::{Context, Result, bail};
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

/// Examina el índice de `dir`. Los `Field` se reconstruyen con `build_schema`,
/// así que un índice con otro esquema haría que cada valor acabase en el campo
/// equivocado: por eso se distingue como `Outdated`.
pub fn inspect_index(dir: &Path) -> Result<IndexStatus> {
    if !dir.join("meta.json").is_file() {
        return Ok(IndexStatus::Missing);
    }

    let index = Index::open_in_dir(dir).with_context(|| {
        format!(
            "no se pudo abrir el índice en {} (si está dañado, vacía el directorio y \
             ejecuta `reindex`)",
            dir.display()
        )
    })?;

    let (schema, fields) = build_schema();
    if index.schema() != schema {
        return Ok(IndexStatus::Outdated);
    }

    register_tokenizers(&index);
    Ok(IndexStatus::Ready(index, fields))
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
mod tests {
    use super::*;
    use tantivy::schema::TEXT;

    #[test]
    fn open_index_requires_an_existing_index() {
        let tmp = tempfile::tempdir().unwrap();
        let Err(err) = open_index(tmp.path()) else {
            panic!("no debería abrir un directorio vacío");
        };
        assert!(err.to_string().contains("reindex"), "{err}");
    }

    #[test]
    fn open_index_rejects_indexes_built_with_another_schema() {
        // Un índice de una versión anterior (otro analizador en los campos de
        // texto) tendría términos que ya no coinciden con los de las consultas.
        let tmp = tempfile::tempdir().unwrap();
        let mut builder = Schema::builder();
        builder.add_text_field("id", STRING | STORED);
        builder.add_text_field("titulo", TEXT | STORED);
        Index::create_in_dir(tmp.path(), builder.build()).unwrap();

        let Err(err) = open_index(tmp.path()) else {
            panic!("no debería abrir un índice con otro esquema");
        };
        assert!(err.to_string().contains("reindex"), "{err}");
    }

    #[test]
    fn open_index_accepts_indexes_built_by_this_version() {
        let tmp = tempfile::tempdir().unwrap();
        create_index_in(tmp.path()).unwrap();
        open_index(tmp.path()).unwrap();
    }
}
