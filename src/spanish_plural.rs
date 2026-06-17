use tantivy::tokenizer::{Token, TokenFilter, TokenStream, Tokenizer};

/// Filtro que normaliza plurales españoles quitando la terminación `-es` o `-s`.
///
/// Pensado para ir DESPUÉS del paso a minúsculas y del plegado de acentos, de
/// modo que "actualizacion", "actualización" y "actualizaciones" colapsen al
/// mismo término. Es deliberadamente simple (favorece el recall) frente al
/// stemmer snowball, que depende de las tildes y no unifica el singular sin
/// tilde con el plural. Al aplicarse igual al indexar y al consultar, las
/// búsquedas quedan coherentes.
#[derive(Clone)]
pub struct SpanishPluralFilter;

impl TokenFilter for SpanishPluralFilter {
    type Tokenizer<T: Tokenizer> = SpanishPluralFilterWrapper<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> Self::Tokenizer<T> {
        SpanishPluralFilterWrapper { tokenizer }
    }
}

#[derive(Clone)]
pub struct SpanishPluralFilterWrapper<T> {
    tokenizer: T,
}

impl<T: Tokenizer> Tokenizer for SpanishPluralFilterWrapper<T> {
    type TokenStream<'a> = SpanishPluralTokenStream<T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        SpanishPluralTokenStream {
            tail: self.tokenizer.token_stream(text),
        }
    }
}

pub struct SpanishPluralTokenStream<T> {
    tail: T,
}

/// Quita el plural en sitio. A estas alturas el token ya es ASCII (tras el
/// folding), así que truncar por bytes es seguro. Conservamos al menos 3
/// caracteres de raíz para no destrozar palabras cortas.
fn strip_plural(text: &mut String) {
    if text.len() >= 5 && text.ends_with("es") {
        text.truncate(text.len() - 2);
    } else if text.len() >= 4 && text.ends_with('s') {
        text.truncate(text.len() - 1);
    }
}

impl<T: TokenStream> TokenStream for SpanishPluralTokenStream<T> {
    fn advance(&mut self) -> bool {
        if !self.tail.advance() {
            return false;
        }
        strip_plural(&mut self.token_mut().text);
        true
    }

    fn token(&self) -> &Token {
        self.tail.token()
    }

    fn token_mut(&mut self) -> &mut Token {
        self.tail.token_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::SpanishPluralFilter;
    use tantivy::tokenizer::{
        AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer,
    };

    fn tokens(text: &str) -> Vec<String> {
        let mut analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(RemoveLongFilter::limit(40))
            .filter(LowerCaser)
            .filter(AsciiFoldingFilter)
            .filter(SpanishPluralFilter)
            .build();
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        out
    }

    #[test]
    fn collapses_accents_case_and_plural() {
        // Las cuatro variantes deben producir el mismo término único.
        for w in ["actualizacion", "actualización", "actualizaciones", "ACTUALIZACIONES"] {
            assert_eq!(tokens(w), vec!["actualizacion".to_string()], "falló: {w}");
        }
        // Singular/plural normal también colapsan.
        assert_eq!(tokens("noticias"), tokens("noticia"));
        // No destrozamos palabras cortas.
        assert_eq!(tokens("mes"), vec!["mes".to_string()]);
    }
}
