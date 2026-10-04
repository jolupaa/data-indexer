use anyhow::Result;
use std::process::ExitCode;

mod acl;
mod error;
mod indexer;
mod init;
mod nfc;
mod reindex;
mod searcher;
mod server;
mod spanish_plural;
mod state;
mod utils;

#[cfg(test)]
mod api_tests;

use reindex::reindex;
use server::start_server;

const USAGE: &str = "\
Uso:
  data-indexer reindex   Reconstruye el índice desde PostgreSQL (necesita DB_URL)
  data-indexer serve     Sirve el índice por HTTP (comando por defecto)

Variables de entorno:
  DB_URL      Cadena de conexión a PostgreSQL (sólo `reindex`)
  INDEX_DIR   Directorio del índice (por defecto ./search_index)
  BIND_ADDR   Dirección de escucha de `serve` (por defecto 127.0.0.1:5000)";

#[tokio::main]
async fn main() -> Result<ExitCode> {
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

        "help" | "-h" | "--help" => {
            println!("{USAGE}");
        }

        _ => {
            eprintln!("Comando no reconocido: {}", command);
            eprintln!("{USAGE}");
            return Ok(ExitCode::from(2));
        }
    }

    Ok(ExitCode::SUCCESS)
}
