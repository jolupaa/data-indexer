use tantivy::tokenizer::{Token, TokenFilter, TokenStream, Tokenizer};

/// Filtro que reduce singular y plural españoles a una misma raíz.
///
/// Pensado para ir DESPUÉS del paso a minúsculas y del plegado de acentos, de
/// modo que "actualizacion", "actualización" y "actualizaciones" colapsen al
/// mismo término. Es deliberadamente simple (favorece el recall) frente al
/// stemmer snowball, que depende de las tildes y no unifica el singular sin
/// tilde con el plural. Al aplicarse igual al indexar y al consultar, las
/// búsquedas quedan coherentes.
///
/// El término resultante no siempre es una palabra real ("clases" → "clas"):
/// lo que importa es que el singular y el plural produzcan el mismo.
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

/// Normaliza el número en sitio. Sólo recorta terminaciones ASCII, así que
/// truncar por bytes es seguro aunque el token conserve caracteres no ASCII.
/// Conservamos al menos 3 caracteres de raíz para no destrozar palabras cortas.
///
/// - `-es` → se quita ("canciones" → "cancion", "clases" → "clas"). Si queda
///   otra vez `-es` se quita de nuevo, porque el singular también acaba así
///   ("intereses" → "interes" → "inter", igual que "interés").
/// - `-as` / `-os` → se quita la `s` ("noticias" → "noticia"). No tocamos
///   `-is` / `-us`, que suelen ser singulares ("país", "virus", "crisis").
/// - Palabras de 4 letras en `-es` → se quita la `s` ("pies" → "pie").
/// - Singular en `-e` → se quita la `e` para que coincida con su plural en
///   `-es` ("clase" → "clas", "presidente" → "president").
/// - Una `z` final pasa a `c`, que es como queda el plural sin `-es`
///   ("luz" / "luces" → "luc", "vez" / "veces" → "vec").
fn strip_plural(text: &mut String) {
    let len = text.len();
    if len >= 5 && text.ends_with("es") {
        text.truncate(len - 2);
        if text.len() >= 5 && text.ends_with("es") {
            text.truncate(text.len() - 2);
        }
    } else if len >= 4
        && (text.ends_with("as")
            || text.ends_with("os")
            || text.ends_with("es")
            || text.ends_with('e'))
    {
        // -as/-os (y -es en palabras de 4 letras): fuera la `s`; singular en -e: fuera la `e`.
        text.truncate(len - 1);
    }

    if text.ends_with('z') {
        text.pop();
        text.push('c');
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
        for w in [
            "actualizacion",
            "actualización",
            "actualizaciones",
            "ACTUALIZACIONES",
        ] {
            assert_eq!(tokens(w), vec!["actualizacion".to_string()], "falló: {w}");
        }
        // Singular/plural normal también colapsan.
        assert_eq!(tokens("noticias"), tokens("noticia"));
        // No destrozamos palabras cortas.
        assert_eq!(tokens("mes"), vec!["mes".to_string()]);
    }

    #[test]
    fn singular_and_plural_share_a_term() {
        let pairs = [
            ("noticia", "noticias"),
            ("gato", "gatos"),
            ("día", "días"),
            ("canción", "canciones"),
            ("ciudad", "ciudades"),
            ("mes", "meses"),
            ("ley", "leyes"),
            ("clase", "clases"),
            ("presidente", "presidentes"),
            ("informe", "informes"),
            ("base", "bases"),
            ("pie", "pies"),
            ("café", "cafés"),
            ("serie", "series"),
            ("luz", "luces"),
            ("vez", "veces"),
            ("lápiz", "lápices"),
            ("dulce", "dulces"),
            ("avance", "avances"),
            ("país", "países"),
            ("autobús", "autobuses"),
            ("interés", "intereses"),
            ("inglés", "ingleses"),
            ("rubí", "rubíes"),
        ];
        for (singular, plural) in pairs {
            let s = tokens(singular);
            assert_eq!(s.len(), 1, "{singular} debería dar un único término");
            assert_eq!(s, tokens(plural), "{singular} / {plural} no colapsan");
        }
    }

    #[test]
    fn keeps_short_words_and_singulars_in_s() {
        for w in ["mes", "tres", "gas", "los", "virus", "crisis", "análisis"] {
            assert!(tokens(w)[0].len() >= 3, "{w} quedó demasiado corto");
        }
        assert_eq!(tokens("virus"), vec!["virus".to_string()]);
        assert_eq!(tokens("crisis"), vec!["crisis".to_string()]);
        assert_eq!(tokens("país"), vec!["pais".to_string()]);
    }
}
