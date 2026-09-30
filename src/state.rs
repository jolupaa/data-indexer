use std::sync::{Arc, Mutex, PoisonError};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, query::QueryParser};

use crate::error::ApiError;
use crate::init::SearchFields;

/// Memoria del `IndexWriter` del servidor.
const WRITER_MEMORY_BYTES: usize = 50_000_000;

#[derive(Clone)]
pub struct AppState {
    pub index: Index,
    /// Lector compartido por todas las búsquedas. Crear uno por petición
    /// obliga a reabrir todos los segmentos cada vez.
    pub reader: IndexReader,
    pub writer: Arc<Mutex<IndexWriter>>,
    pub query_parser: Arc<QueryParser>,
    pub fields: SearchFields,
}

impl AppState {
    pub fn new(index: Index, fields: SearchFields) -> tantivy::Result<Self> {
        let writer: IndexWriter = index.writer(WRITER_MEMORY_BYTES)?;
        // Se recarga solo cuando cambia el índice (p. ej. al terminar una fusión
        // de segmentos) y, además, a mano tras cada commit de la API.
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let query_parser = QueryParser::for_index(&index, fields.full_text());

        Ok(Self {
            index,
            reader,
            writer: Arc::new(Mutex::new(writer)),
            query_parser: Arc::new(query_parser),
            fields,
        })
    }

    /// Aplica `ops` con el writer, confirma los cambios y recarga el lector para
    /// que la siguiente búsqueda ya los vea. Si algo falla, descarta todo lo
    /// pendiente para que no acabe colándose a medias en el siguiente commit.
    ///
    /// Es bloqueante (el commit escribe y sincroniza a disco), así que se ejecuta
    /// fuera del runtime async.
    pub async fn write<F>(&self, ops: F) -> Result<(), ApiError>
    where
        F: FnOnce(&IndexWriter) -> tantivy::Result<()> + Send + 'static,
    {
        let state = self.clone();
        tokio::task::spawn_blocking(move || state.write_blocking(ops))
            .await
            .map_err(ApiError::internal)?
            .map_err(ApiError::internal)
    }

    fn write_blocking<F>(&self, ops: F) -> tantivy::Result<()>
    where
        F: FnOnce(&IndexWriter) -> tantivy::Result<()>,
    {
        // Un pánico en otra petición no debe dejar el servidor sin escrituras
        // para siempre: las operaciones del writer no quedan a medias.
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);

        let result = ops(&writer).and_then(|()| writer.commit().map(|_| ()));
        if let Err(err) = result {
            if let Err(rollback_err) = writer.rollback() {
                eprintln!("Error al deshacer cambios pendientes: {rollback_err}");
            }
            return Err(err);
        }
        drop(writer);

        self.reader.reload()
    }
}
