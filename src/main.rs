use anyhow::Result;
mod lib;

mod indexer;
use indexer::*;

mod searcher;
use searcher::*;

mod init;
use init::*;

mod server;
use server::*;

#[tokio::main]
async fn main() -> Result<()> {
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "serve".to_string());

    match command.as_str() {
        "reindex" => {
            println!("Reconstruyendo índice completo...");
            reindex().await?;
            println!("Indexación inicial completada.");
        }

        "serve" => {
            println!("Arrancando servidor de búsqueda...");
            start_server().await?;
        }

        _ => {
            eprintln!("Comando no reconocido: {}", command);
            eprintln!("Uso:");
            eprintln!("  cargo run -- reindex");
            eprintln!("  cargo run -- serve");
        }
    }

    Ok(())
}