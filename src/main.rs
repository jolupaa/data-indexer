use anyhow::Result;
mod indexers;

use indexers::*;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    start_indexing().await?;

    println!("Datos indexados correctamente");

    Ok(())
}