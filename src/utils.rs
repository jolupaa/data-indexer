pub fn make_uid(tipo: &str, id: &str) -> String {
    format!("{}:{}", tipo, id)
}
