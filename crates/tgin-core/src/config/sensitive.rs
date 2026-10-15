//! Resolved URLs and paths, which may carry credentials.

use std::fmt;
use std::ops::Range;
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

/// A run of written text: a `${…}` reference, or literal text.
struct Run {
    range: Range<usize>,
    reference: bool,
}

/// Splits `text` the way `interpolate` reads it: `${…}` up to the first `}`
/// is a reference; `$${` and an unterminated `${` are literal.
fn runs(text: &str) -> Vec<Run> {
    let mut runs = Vec::new();
    let (mut i, mut literal) = (0, 0);
    while let Some(found) = text[i..].find('$') {
        let at = i + found;
        let tail = &text[at..];
        if tail.starts_with("$${") {
            i = at + 3;
        } else if let Some(body) = tail.strip_prefix("${")
            && let Some(close) = body.find('}')
        {
            if literal < at {
                runs.push(Run {
                    range: literal..at,
                    reference: false,
                });
            }
            let end = at + 2 + close + 1;
            runs.push(Run {
                range: at..end,
                reference: true,
            });
            i = end;
            literal = end;
        } else {
            i = at + 1;
        }
    }
    if literal < text.len() {
        runs.push(Run {
            range: literal..text.len(),
            reference: false,
        });
    }
    runs
}

/// The first position at or after `from`, outside references, of a byte in
/// `set`.
fn find_literal(text: &str, runs: &[Run], from: usize, set: &[u8]) -> Option<usize> {
    runs.iter()
        .filter(|run| !run.reference && run.range.end > from)
        .find_map(|run| {
            let start = run.range.start.max(from);
            text.as_bytes()[start..run.range.end]
                .iter()
                .position(|b| set.contains(b))
                .map(|p| start + p)
        })
}

/// The last position in `range`, outside references, of `byte`.
fn rfind_literal(text: &str, runs: &[Run], range: Range<usize>, byte: u8) -> Option<usize> {
    runs.iter()
        .rev()
        .filter(|run| !run.reference)
        .find_map(|run| {
            let start = run.range.start.max(range.start);
            let end = run.range.end.min(range.end);
            (start < end)
                .then(|| text.as_bytes()[start..end].iter().rposition(|b| *b == byte))
                .flatten()
                .map(|p| start + p)
        })
}

/// Text as written in the config, for `Debug` and `/v1/config`, without
/// parsing it (a `${…}` in the host would defeat a URL parser).
///
/// In text with `://`, the userinfo (up to the authority's last `@`) and
/// everything after the first `?` or `#` are masked as `[redacted]`. Then
/// bot tokens and the redactor's secrets in the remaining literal text are
/// masked as `#####`. `${…}` references are kept verbatim: they name where
/// a value comes from, not the value.
pub(super) fn mask_written(text: &str, redactor: &Redactor) -> String {
    let runs = runs(text);
    let mut hidden: Vec<Range<usize>> = Vec::new();
    let scheme = runs.iter().filter(|run| !run.reference).find_map(|run| {
        text[run.range.clone()]
            .find("://")
            .map(|p| run.range.start + p)
    });
    if let Some(scheme) = scheme {
        let authority = scheme + 3;
        let authority_end = find_literal(text, &runs, authority, b"/?#").unwrap_or(text.len());
        if let Some(at) = rfind_literal(text, &runs, authority..authority_end, b'@') {
            hidden.push(authority..at);
        }
        if let Some(query) = find_literal(text, &runs, authority_end, b"?#") {
            hidden.push(query + 1..text.len());
        }
    }

    let mut out = String::with_capacity(text.len());
    for run in &runs {
        if run.reference {
            out.push_str(&text[run.range.clone()]);
            continue;
        }
        // Cut the literal run at the hidden ranges' edges.
        let mut cuts = vec![run.range.start, run.range.end];
        for range in &hidden {
            for edge in [range.start, range.end] {
                if run.range.contains(&edge) {
                    cuts.push(edge);
                }
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        for piece in cuts.windows(2) {
            let piece = piece[0]..piece[1];
            if hidden.iter().any(|h| h.contains(&piece.start)) {
                out.push_str(REDACTED);
            } else {
                out.push_str(&redactor.redact(&text[piece]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(text: &str) -> String {
        mask_written(text, &Redactor::new(["configured-secret".to_owned()]))
    }

    #[test]
    fn urls_lose_credentials() {
        assert_eq!(
            mask("https://user:pass@host:8443/p?key=v#frag"),
            "https://[redacted]@host:8443/p?[redacted]"
        );
        assert_eq!(mask("http://h/p#frag"), "http://h/p#[redacted]");
        assert_eq!(mask("http://u@h"), "http://[redacted]@h");
        assert_eq!(
            mask("https://api.telegram.org/bot123456789:AAbbccddeeffgghhiijjkkllmmnnoopp_qq/x"),
            "https://api.telegram.org/bot#####/x"
        );
        assert_eq!(mask("http://h/configured-secret/x"), "http://h/#####/x");
        assert_eq!(mask("http://h/"), "http://h/");
    }

    #[test]
    fn written_text_kept_as_written() {
        // No parsing: case, default ports and references survive.
        assert_eq!(
            mask("HTTPS://Host.Example:443/A"),
            "HTTPS://Host.Example:443/A"
        );
        assert_eq!(
            mask("https://api.telegram.org/bot${env:TOKEN}/sendMessage"),
            "https://api.telegram.org/bot${env:TOKEN}/sendMessage"
        );
        assert_eq!(mask("/tls/key.pem"), "/tls/key.pem");
        assert_eq!(mask("${env:URL}"), "${env:URL}");
        assert_eq!(mask("/hook/configured-secret"), "/hook/#####");
    }

    #[test]
    fn references_do_not_hide_credentials() {
        assert_eq!(
            mask("https://${env:HOOK_HOST}/hook?token=literal-query-secret"),
            "https://${env:HOOK_HOST}/hook?[redacted]"
        );
        assert_eq!(
            mask("http://admin:pa55w0rd-literal@${env:HOST}:8080/hook"),
            "http://[redacted]@${env:HOST}:8080/hook"
        );
        // References inside hidden parts stay; the literal text around them
        // does not.
        assert_eq!(
            mask("http://${env:USER}:pw@h/p?k=${env:Q}&x=lit"),
            "http://${env:USER}[redacted]@h/p?[redacted]${env:Q}[redacted]"
        );
        // A `/` inside a reference does not end the authority.
        assert_eq!(
            mask("http://u:p@${file:/run/host}/x"),
            "http://[redacted]@${file:/run/host}/x"
        );
        // `$${` is a literal `${`.
        assert_eq!(mask("http://h/$${x}?q"), "http://h/$${x}?[redacted]");
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
