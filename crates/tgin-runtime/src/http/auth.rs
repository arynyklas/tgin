//! Bearer-token checks for the admin plane and the pull endpoints.

use axum::http::{HeaderMap, header};

use crate::secret::{Secret, secret_eq};

/// Whether `headers` carry `Authorization: Bearer <expected>`; the scheme is
/// matched case-insensitively, the token in constant time.
pub fn bearer_matches(headers: &HeaderMap, expected: &Secret<String>) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let value = value.as_bytes();
    let scheme = b"bearer ";
    value.len() > scheme.len()
        && value[..scheme.len()].eq_ignore_ascii_case(scheme)
        && secret_eq(&value[scheme.len()..], expected.expose().as_bytes())
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn bearer_needs_scheme_and_exact_token() {
        let expected = Secret::new("pull-secret-123".to_owned());
        let check = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, HeaderValue::from_static(value));
            bearer_matches(&headers, &expected)
        };
        assert!(check("Bearer pull-secret-123"));
        assert!(check("bearer pull-secret-123"));
        assert!(!check("Bearer pull-secret-12"));
        assert!(!check("Bearer pull-secret-1234"));
        assert!(!check("Basic pull-secret-123"));
        assert!(!check("pull-secret-123"));
        assert!(!bearer_matches(&HeaderMap::new(), &expected));
    }
}
