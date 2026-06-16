use anyhow::Result;


mod indexer;
use indexer::*;

mod searcher;
use searcher::*;

mod init;
use init::*;

#[tokio::main]
async fn main() -> Result<()> {
    let state = AppState {
        writer: Arc::new(Mutex::new(index_writer)),
        fields,
    };

    let app = Router::new()
        .route("/index/upsert", post(upsert_document))
        .route("/index/delete", delete(delete_document))
        .route("/search", get(search_documents))
        .route("/index/init", post(start_indexing()))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:4000").await?;

    axum::serve(listener, app).await?;

    Ok(())
}