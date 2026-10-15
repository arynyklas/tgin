//! Resolved URLs and paths, which may carry credentials.

use std::fmt;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use tgin_runtime::secret::Redactor;
use url::Url;

use super::model::REDACTED;

/// A resolved URL or path that may carry credentials: a password, path or
/// query in a URL, a bot token in a path, or text read from `${env:…}`.
///
/// `Debug` shows a masked rendering ([`masked`](Self::masked)):
/// - a URL as its origin only, `scheme://host[:port]`, followed by `/…`
///   when it has a path, query or fragment, none of which is shown;
/// - a path as written, `${…}` references unexpanded, with bot tokens and
///   the config's secrets masked.
///
/// [`expose`](Self::expose) gives the resolved value.
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
}

impl Sensitive<Url> {
    pub(super) fn mask(&mut self, redactor: &Redactor) {
        self.shown = origin(&self.value, redactor);
    }
}

impl Sensitive<PathBuf> {
    pub(super) fn mask(&mut self, redactor: &Redactor) {
        self.shown = mask_text(&self.written, redactor);
    }
}

impl Sensitive<String> {
    pub(super) fn mask(&mut self, redactor: &Redactor) {
        self.shown = mask_text(&self.written, redactor);
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

impl<T: Eq> Eq for Sensitive<T> {}

/// Masks text bound for `/v1/config` and `Debug`: bot tokens and the
/// config's own secrets.
#[derive(Clone)]
pub(super) struct Masker(Arc<Redactor>);

impl Masker {
    pub(super) fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        Self(Arc::new(Redactor::new(secrets)))
    }

    pub(super) fn mask(&self, text: &str) -> String {
        mask_text(text, &self.0)
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

/// The parsed URL's origin, `scheme://host[:port]`, then `/…` if anything
/// follows it. Userinfo, path, query and fragment are never shown: only
/// the parser knows where they are, so the rendering starts from it.
fn origin(url: &Url, redactor: &Redactor) -> String {
    let mut shown = String::new();
    let _ = write!(shown, "{}://", url.scheme());
    if let Some(host) = url.host_str() {
        shown.push_str(host);
    }
    if let Some(port) = url.port() {
        let _ = write!(shown, ":{port}");
    }
    let elided = url.path() != "/" || url.query().is_some() || url.fragment().is_some();
    if elided {
        shown.push_str("/…");
    }
    redactor.redact(&shown).into_owned()
}

/// Text as written, `${…}` references verbatim (they name where a value
/// comes from, not the value), with bot tokens and the redactor's secrets
/// masked in the literal text around them.
fn mask_text(text: &str, redactor: &Redactor) -> String {
    let mut out = String::with_capacity(text.len());
    let mut literal = 0;
    let mut i = 0;
    while let Some(found) = text[i..].find('$') {
        let at = i + found;
        let tail = &text[at..];
        if tail.starts_with("$${") {
            i = at + 3;
        } else if let Some(body) = tail.strip_prefix("${")
            && let Some(close) = body.find('}')
        {
            out.push_str(&redactor.redact(&text[literal..at]));
            let end = at + 2 + close + 1;
            out.push_str(&text[at..end]);
            i = end;
            literal = end;
        } else {
            i = at + 1;
        }
    }
    out.push_str(&redactor.redact(&text[literal..]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor() -> Redactor {
        Redactor::new(["configured-secret".to_owned()])
    }

    fn shown(url: &str) -> String {
        origin(&Url::parse(url).unwrap(), &redactor())
    }

    #[test]
    fn urls_show_origin_only() {
        for (url, expected) in [
            (
                "https://user:pass@host:8443/p?key=v#frag",
                "https://host:8443/…",
            ),
            ("http://h/", "http://h"),
            ("http://h", "http://h"),
            ("http://h/?q", "http://h/…"),
            ("http://h/#f", "http://h/…"),
            ("HTTPS://Host.Example:443/", "https://host.example"),
            ("http://[::1]:8080/x", "http://[::1]:8080/…"),
            (
                "https:///user:pa55w0rd-literal@push.example/x",
                "https://push.example/…",
            ),
            ("https:////host/x", "https://host/…"),
            ("http://h/cb?token=lit&next=http://x/", "http://h/…"),
            ("http://h/путь/ключ", "http://h/…"),
            (
                "https://api.telegram.org/bot123456789:AAbbccddeeffgghhiijjkkllmmnnoopp_qq/x",
                "https://api.telegram.org/…",
            ),
            ("http://configured-secret.example/", "http://#####.example"),
        ] {
            assert_eq!(shown(url), expected, "{url}");
        }
    }

    #[test]
    fn text_keeps_references() {
        let mask = |s: &str| mask_text(s, &redactor());
        assert_eq!(mask("/tls/key.pem"), "/tls/key.pem");
        assert_eq!(mask("${env:URL}"), "${env:URL}");
        assert_eq!(mask("/hook/configured-secret"), "/hook/#####");
        assert_eq!(
            mask("/hook/${env:SEGMENT}/configured-secret"),
            "/hook/${env:SEGMENT}/#####"
        );
        assert_eq!(
            mask("/bot123456789:AAbbccddeeffgghhiijjkkllmmnnoopp_qq/x"),
            "/bot#####/x"
        );
        // `$${` is a literal `${`, and an unterminated `${` is literal text.
        assert_eq!(mask("/a/$${configured-secret}"), "/a/$${#####}");
        assert_eq!(mask("/a/${configured-secret"), "/a/${#####");
    }

    #[test]
    fn debug_is_masked_and_starts_redacted() {
        let mut url = Sensitive::new(
            Url::parse("https://u:p@h/x?q=1").unwrap(),
            "https://u:p@h/x?q=1",
        );
        assert_eq!(format!("{url:?}"), r#""[redacted]""#);
        url.mask(&Redactor::new(Vec::new()));
        assert_eq!(format!("{url:?}"), r#""https://h/…""#);
        assert_eq!(url.expose().password(), Some("p"));
    }
}
