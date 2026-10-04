//! Principales de acceso (ACL): quién puede ver cada documento.
//!
//! Un documento lleva uno o más principales en el campo `acl` y `/search` sólo
//! devuelve los que comparten alguno con los de la consulta. Los valores son
//! opacos para el indexer; el backend usa `public`, `u:<users.id>` y
//! `a:<users.id>`.

use crate::error::ApiError;

/// Principal de los documentos públicos: el de un documento que llega sin
/// `acl` y el de una búsqueda sin `acl`.
pub const PUBLIC: &str = "public";
/// Máximo de principales de un documento.
pub const MAX_DOCUMENT_PRINCIPALS: usize = 32;
/// Longitud máxima de un principal, en bytes.
pub const MAX_PRINCIPAL_BYTES: usize = 256;
/// Máximo de principales de una búsqueda.
pub const MAX_SEARCH_PRINCIPALS: usize = 64;

/// Un principal tiene de 1 a 256 bytes, sin espacios en blanco ni comas (la
/// coma separa los principales en `/search?acl=`). El mensaje de error no
/// repite el valor, que puede ser enorme.
pub fn validate_principal(value: &str) -> Result<(), ApiError> {
    if value.is_empty() || value.len() > MAX_PRINCIPAL_BYTES {
        return Err(ApiError::bad_request(format!(
            "cada valor de `acl` debe tener entre 1 y {MAX_PRINCIPAL_BYTES} bytes"
        )));
    }
    if value.chars().any(|c| c.is_whitespace() || c == ',') {
        return Err(ApiError::bad_request(
            "los valores de `acl` no pueden contener espacios ni comas",
        ));
    }
    Ok(())
}

/// Valida la `acl` de un documento. Vacía es válida: el documento será público.
pub fn validate_document_acl(acl: &[String]) -> Result<(), ApiError> {
    if acl.len() > MAX_DOCUMENT_PRINCIPALS {
        return Err(ApiError::bad_request(format!(
            "`acl` no puede tener más de {MAX_DOCUMENT_PRINCIPALS} valores"
        )));
    }
    acl.iter().try_for_each(|value| validate_principal(value))
}

/// Principales de `/search?acl=`, separados por comas. Sin el parámetro, sólo
/// `public`. Un valor vacío (`acl=`, `acl=a,,b`, una coma al final) es un
/// error, no "público": quien manda `acl` quiere restringir, y adivinar qué
/// quiso decir podría enseñarle de más o de menos.
pub fn search_principals(acl: Option<&str>) -> Result<Vec<String>, ApiError> {
    let Some(acl) = acl else {
        return Ok(vec![PUBLIC.to_string()]);
    };
    let values: Vec<&str> = acl.split(',').collect();
    if values.len() > MAX_SEARCH_PRINCIPALS {
        return Err(ApiError::bad_request(format!(
            "`acl` no puede tener más de {MAX_SEARCH_PRINCIPALS} valores"
        )));
    }
    values
        .into_iter()
        .map(|value| validate_principal(value).map(|()| value.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_principals_of_the_contract() {
        let longest = "x".repeat(MAX_PRINCIPAL_BYTES);
        let accented = "é".repeat(MAX_PRINCIPAL_BYTES / 2);
        for value in [
            "public",
            "u:usr-emp-001",
            "a:usr-admin-001",
            &longest,
            &accented,
        ] {
            validate_principal(value).unwrap();
        }
    }

    #[test]
    fn rejects_empty_oversized_blank_and_comma_principals() {
        let too_long = "x".repeat(MAX_PRINCIPAL_BYTES + 1);
        // 129 × 2 bytes: el límite es de bytes, no de caracteres.
        let too_long_accented = "é".repeat(MAX_PRINCIPAL_BYTES / 2 + 1);
        for value in [
            "",
            &too_long,
            &too_long_accented,
            "u:a b",
            "u:a\tb",
            "u:a\n",
            " u:a",
            "u:a\u{a0}b",
            "u:a,b",
        ] {
            assert!(validate_principal(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn a_search_without_acl_is_public() {
        assert_eq!(search_principals(None).unwrap(), ["public"]);
    }

    #[test]
    fn search_principals_are_comma_separated_and_validated() {
        assert_eq!(
            search_principals(Some("u:usr-emp-001,a:usr-emp-001")).unwrap(),
            ["u:usr-emp-001", "a:usr-emp-001"]
        );
        let at_the_limit = vec!["u:1"; MAX_SEARCH_PRINCIPALS].join(",");
        assert_eq!(
            search_principals(Some(&at_the_limit)).unwrap().len(),
            MAX_SEARCH_PRINCIPALS
        );
        let too_many = vec!["u:1"; MAX_SEARCH_PRINCIPALS + 1].join(",");
        for bad in ["", ",", "u:1,", ",u:1", "u:1,,u:2", "u:1, a:1", &too_many] {
            assert!(search_principals(Some(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_document_takes_at_most_32_principals() {
        let principals = |n: usize| (0..n).map(|i| format!("u:{i}")).collect::<Vec<_>>();
        validate_document_acl(&[]).unwrap();
        validate_document_acl(&principals(MAX_DOCUMENT_PRINCIPALS)).unwrap();
        assert!(validate_document_acl(&principals(MAX_DOCUMENT_PRINCIPALS + 1)).is_err());
        assert!(validate_document_acl(&["u:1".into(), "".into()]).is_err());
    }
}
