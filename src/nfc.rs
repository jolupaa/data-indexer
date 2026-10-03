use tantivy::tokenizer::Tokenizer;
use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfc_quick};

/// Tokenizer que recompone el texto en forma NFC antes de delegar en `T`.
///
/// Un texto en forma descompuesta (NFD: "o" + tilde combinante), habitual en
/// texto extraído de PDFs o pegado desde macOS, haría que el tokenizer cortase
/// la palabra en la tilde ("actualizacio" + "n"). Recomponiéndolo, "ó" llega
/// entera y el plegado de acentos posterior la deja en "o". Al formar parte del
/// analizador se aplica igual al indexar y al consultar.
#[derive(Clone)]
pub struct NfcTokenizer<T> {
    inner: T,
    buffer: String,
}

impl<T> NfcTokenizer<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            buffer: String::new(),
        }
    }
}

impl<T: Tokenizer> Tokenizer for NfcTokenizer<T> {
    type TokenStream<'a> = T::TokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        // Casi todo el texto ya viene en NFC: en ese caso no copiamos nada.
        if is_nfc_quick(text.chars()) == IsNormalized::Yes {
            return self.inner.token_stream(text);
        }
        self.buffer.clear();
        self.buffer.extend(text.nfc());
        self.inner.token_stream(&self.buffer)
    }
}
