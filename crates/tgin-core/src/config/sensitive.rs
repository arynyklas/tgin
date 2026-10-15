//! Resolved URLs and paths, which may carry credentials.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use tgin_runtime::secret::Redactor;
use url::Url;

use super::model::REDACTED;

/// A resolved URL or path that may carry credentials: a password or query
/// in a URL, a bot token in a path, or text read from `${env:…}`.
///
/// `Debug` shows the field as written, `${…}` references unexpanded, with
/// URL userinfo, query and fragment, bot tokens and configured secrets
/// masked. [`expose`](Self::expose) gives the resolved value.
#[derive(Clone)]
pub struct Sensitive<T> {
    value: T,
    written: String,
    /// The masked rendering; `[redacted]` until [`Sensitive::mask`] runs.
    shown: String,
}

/// A resolved `http` or `https` URL.
pub type SensitiveUrl = Sensitive<Url>;
/// A resolved file path.
pub type SensitivePath = Sensitive<PathBuf>;
/// A resolved HTTP route path such as `/webhook/shop`.
pub type SensitiveRoute = Sensitive<String>;

impl<T> Sensitive<T> {
    pub(super) fn new(value: T, written: &str) -> Self {
        Self {
            value,
            written: written.to_owned(),
            shown: REDACTED.to_owned(),
        }
    }

    /// The resolved value; callers own keeping it out of logs.
    pub fn expose(&self) -> &T {
        &self.value
    }

    /// The masked rendering that `Debug` shows.
    pub fn masked(&self) -> &str {
        &self.shown
    }

    pub(super) fn mask(&mut self, redactor: &Redactor) {
        self.shown = mask_written(&self.written, redactor);
    }
}

impl<T> fmt::Debug for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.shown.as_str(), f)
    }
}

impl<T: PartialEq> PartialEq for Sensitive<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

/// Masks text bound for `/v1/config` and `Debug`: URL credentials, bot
/// tokens and the config's own secrets.
#[derive(Clone)]
pub(super) struct Masker(Arc<Redactor>);

impl Masker {
    pub(super) fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        Self(Arc::new(Redactor::new(secrets)))
    }

    pub(super) fn mask(&self, text: &str) -> String {
        mask_written(text, &self.0)
    }

    pub(super) fn redactor(&self) -> &Redactor {
        &self.0
    }
}

impl fmt::Debug for Masker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Masker")
    }
}

impl<T: Eq> Eq for Sensitive<T> {}

/// `url` without userinfo, query and fragment, then with bot tokens and
/// configured secrets masked.
fn mask_url(url: &Url, redactor: &Redactor) -> String {
    let mut shown = format!("{}://", url.scheme());
    if !url.username().is_empty() || url.password().is_some() {
        shown.push_str(REDACTED);
        shown.push('@');
    }
    if let Some(host) = url.host_str() {
        shown.push_str(host);
    }
    if let Some(port) = url.port() {
        shown.push(':');
        shown.push_str(&port.to_string());
    }
    shown.push_str(url.path());
    if url.query().is_some() {
        shown.push('?');
        shown.push_str(REDACTED);
    }
    if url.fragment().is_some() {
        shown.push('#');
        shown.push_str(REDACTED);
    }
    redactor.redact(&shown).into_owned()
}

/// Text as written in the config: URL-shaped text is masked like
/// [`mask_url`], and bot tokens and configured secrets everywhere.
pub(super) fn mask_written(text: &str, redactor: &Redactor) -> String {
    match Url::parse(text) {
        Ok(url) if text.contains("://") => mask_url(&url, redactor),
        _ => redactor.redact(text).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_lose_credentials() {
        let redactor = Redactor::new(["configured-secret".to_owned()]);
        let mask = |s: &str| mask_written(s, &redactor);
        assert_eq!(
            mask("https://user:pass@host:8443/p?key=v#frag"),
            "https://[redacted]@host:8443/p?[redacted]#[redacted]"
        );
        assert_eq!(
            mask("https://api.telegram.org/bot123456789:AAbbccddeeffgghhiijjkkllmmnnoopp_qq/x"),
            "https://api.telegram.org/bot#####/x"
        );
        assert_eq!(mask("http://h/configured-secret/x"), "http://h/#####/x");
        assert_eq!(mask("http://h/"), "http://h/");
    }

    #[test]
    fn other_text_keeps_shape() {
        let redactor = Redactor::new(["configured-secret".to_owned()]);
        let mask = |s: &str| mask_written(s, &redactor);
        assert_eq!(mask("/tls/key.pem"), "/tls/key.pem");
        assert_eq!(mask("${env:URL}"), "${env:URL}");
        assert_eq!(mask("/hook/configured-secret"), "/hook/#####");
    }

    #[test]
    fn debug_is_masked_and_starts_redacted() {
        let mut url = Sensitive::new(
            Url::parse("https://u:p@h/x?q=1").unwrap(),
            "https://u:p@h/x?q=1",
        );
        assert_eq!(format!("{url:?}"), r#""[redacted]""#);
        url.mask(&Redactor::new(Vec::new()));
        assert_eq!(format!("{url:?}"), r#""https://[redacted]@h/x?[redacted]""#);
        assert_eq!(url.expose().password(), Some("p"));
    }
}
