use std::sync::{Arc, Mutex};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, query::QueryParser};

use crate::error::ApiError;
use crate::init::{SearchFields, WRITER_MEMORY_BYTES};

#[derive(Clone)]
pub struct AppState {
    pub index: Index,
    /// Lector compartido por todas las búsquedas. Crear uno por petición
    /// obliga a reabrir todos los segmentos cada vez.
    pub reader: IndexReader,
    /// `None` si el writer quedó inservible: se crea otro en la siguiente
    /// escritura (ver `discard_pending`).
    pub writer: Arc<Mutex<Option<IndexWriter>>>,
    pub query_parser: Arc<QueryParser>,
    pub fields: SearchFields,
}

impl AppState {
    pub fn new(index: Index, fields: SearchFields) -> tantivy::Result<Self> {
        let writer: IndexWriter = index.writer(WRITER_MEMORY_BYTES)?;
        // Se recarga solo cuando cambia el índice y, además, a mano tras cada
        // commit de la API para que la respuesta ya sea visible. La recarga
        // automática recoge las fusiones de segmentos (y libera los ficheros de
        // los segmentos fusionados) aunque no lleguen más escrituras.
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let query_parser = QueryParser::for_index(&index, fields.full_text());

        Ok(Self {
            index,
            reader,
            writer: Arc::new(Mutex::new(Some(writer))),
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

    /// Descarta las operaciones pendientes del writer. Si el propio rollback
    /// falla, tantivy deja ese writer inservible (sin su lock, y un segundo
    /// rollback entraría en pánico): se sustituye en el acto por uno nuevo, que
    /// recupera el lock del índice. Si tampoco se puede, se deja el hueco vacío
    /// y la siguiente escritura lo vuelve a intentar.
    fn discard_pending(&self, slot: &mut Option<IndexWriter>) {
        let Some(writer) = slot.as_mut() else {
            return;
        };
        let Err(err) = writer.rollback() else {
            return;
        };
        eprintln!("Error al deshacer cambios pendientes; se sustituye el writer: {err}");
        *slot = None;
        match self.index.writer(WRITER_MEMORY_BYTES) {
            Ok(fresh) => *slot = Some(fresh),
            Err(err) => eprintln!("No se pudo recrear el writer del índice: {err}"),
        }
    }

    fn write_blocking<F>(&self, ops: F) -> tantivy::Result<()>
    where
        F: FnOnce(&IndexWriter) -> tantivy::Result<()>,
    {
        let mut slot = match self.writer.lock() {
            Ok(slot) => slot,
            Err(poisoned) => {
                // Otra petición entró en pánico con el writer en la mano y pudo
                // dejar operaciones a medias (p. ej. el borrado de un upsert sin
                // su alta): se descartan en vez de dejar que este commit las
                // confirme, y el servidor sigue aceptando escrituras.
                let mut slot = poisoned.into_inner();
                self.discard_pending(&mut slot);
                self.writer.clear_poison();
                slot
            }
        };

        let writer = match slot.as_mut() {
            Some(writer) => writer,
            None => slot.insert(self.index.writer(WRITER_MEMORY_BYTES)?),
        };
        let result = ops(writer).and_then(|()| writer.commit().map(|_| ()));
        if let Err(err) = result {
            self.discard_pending(&mut slot);
            return Err(err);
        }
        drop(slot);

        // El cambio ya está confirmado en disco: si la recarga falla no es un
        // error de la escritura (la recarga automática lo recogerá enseguida).
        if let Err(err) = self.reader.reload() {
            eprintln!("Error al recargar el lector tras un commit: {err}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::{build_schema, register_tokenizers};
    use tantivy::{Term, collector::Count, doc, query::AllQuery};

    #[tokio::test]
    async fn a_panic_mid_write_does_not_leak_half_applied_ops() {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizers(&index);
        let state = AppState::new(index, fields).unwrap();

        state
            .write(move |writer| {
                writer.add_document(doc!(fields.uid => "noticia:1"))?;
                Ok(())
            })
            .await
            .unwrap();

        // Un "upsert" que borra y revienta antes de añadir el documento nuevo.
        let panicked = state
            .write(move |writer| {
                writer.delete_term(Term::from_field_text(fields.uid, "noticia:1"));
                panic!("fallo a mitad de escritura");
            })
            .await;
        assert!(panicked.is_err());

        // La siguiente escritura funciona y no confirma el borrado huérfano.
        state
            .write(move |writer| {
                writer.add_document(doc!(fields.uid => "noticia:2"))?;
                Ok(())
            })
            .await
            .unwrap();
        let searcher = state.reader.searcher();
        assert_eq!(searcher.search(&AllQuery, &Count).unwrap(), 2);
    }

    #[tokio::test]
    async fn a_discarded_writer_is_replaced_on_the_next_write() {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizers(&index);
        let state = AppState::new(index, fields).unwrap();

        // Lo que hace `discard_pending` cuando el rollback falla.
        *state.writer.lock().unwrap() = None;

        state
            .write(move |writer| {
                writer.add_document(doc!(fields.uid => "noticia:1"))?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            state.reader.searcher().search(&AllQuery, &Count).unwrap(),
            1
        );
        assert!(state.writer.lock().unwrap().is_some());
    }
}
