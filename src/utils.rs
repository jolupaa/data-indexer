use std::path::PathBuf;

pub const DEFAULT_INDEX_DIR: &str = "./search_index";
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:5000";

pub fn make_uid(tipo: &str, id: &str) -> String {
    format!("{}:{}", tipo, id)
}

/// Directorio del índice: `INDEX_DIR` o `./search_index`.
pub fn index_dir() -> PathBuf {
    PathBuf::from(env_or("INDEX_DIR", DEFAULT_INDEX_DIR))
}

/// Dirección en la que escucha `serve`: `BIND_ADDR` o `127.0.0.1:5000`.
pub fn bind_addr() -> String {
    env_or("BIND_ADDR", DEFAULT_BIND_ADDR)
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}
