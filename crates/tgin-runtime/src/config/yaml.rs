//! YAML deserialization with located errors.

use std::borrow::Cow;
use std::fmt::Write as _;

use serde::de::DeserializeOwned;
use serde_path_to_error::Segment;
use serde_saphyr::{DuplicateKeyPolicy, Localizer, Location, SnippetMode, UserMessageFormatter};

use crate::secret::Redactor;

use super::{ConfigError, Located, Severity};

/// Deserializes `text`, rejecting duplicate keys. On failure the error
/// carries the YAML path, line and column of the offending node, and its
/// message is the problem alone: no source text, and token-shaped values it
/// quotes masked.
///
/// # Errors
///
/// Fails if `text` is not valid YAML or does not match `T`.
pub fn parse_yaml<T: DeserializeOwned>(text: &str) -> Result<T, ConfigError> {
    let options = serde_saphyr::options! {
        duplicate_keys: DuplicateKeyPolicy::Error,
    };
    let mut failed_at = None;
    serde_saphyr::with_deserializer_from_str_with_options(text, options, |de| {
        let mut track = serde_path_to_error::Track::new();
        T::deserialize(serde_path_to_error::Deserializer::new(de, &mut track)).inspect_err(|_| {
            failed_at = Some(track.path());
        })
    })
    .map_err(|err| ConfigError {
        severity: Severity::Error,
        at: err.location().map(|location| Located {
            path: failed_at.as_ref().map_or_else(|| ".".to_owned(), yaml_path),
            line: saturate(location.line()),
            column: saturate(location.column()),
        }),
        message: message(&err),
        hint: None,
    })
}

/// The problem, without a location since [`Located`] carries it, and without
/// a source snippet, which would quote the config verbatim.
fn message(err: &serde_saphyr::Error) -> String {
    let formatter = UserMessageFormatter.with_localizer(&NoLocationSuffix);
    let message = err.render_with_options(serde_saphyr::render_options! {
        formatter: &formatter,
        snippets: SnippetMode::Off,
    });
    Redactor::new(std::iter::empty())
        .redact(&message)
        .into_owned()
}

/// Renders messages without serde-saphyr's `at line L, column C` suffix.
struct NoLocationSuffix;

impl Localizer for NoLocationSuffix {
    fn attach_location<'a>(&self, base: Cow<'a, str>, _location: Location) -> Cow<'a, str> {
        base
    }
}

/// `path` as `bots[1].token`; `.` is the document root. Segments the tracker
/// could not name (such as a duplicate key) are left out, and so is every
/// `value` key: [`serde_saphyr::Spanned`] reads its value through a
/// synthesized `value` key, which is not in the document. A user-chosen map
/// key named `value` is left out too; the line and column still point at it.
fn yaml_path(path: &serde_path_to_error::Path) -> String {
    let mut out = String::new();
    for segment in path {
        match segment {
            Segment::Seq { index } => {
                let _ = write!(out, "[{index}]");
            }
            Segment::Map { key: name } | Segment::Enum { variant: name } => {
                if name == SPANNED_VALUE_KEY {
                    continue;
                }
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(name);
            }
            Segment::Unknown => {}
        }
    }
    if out.is_empty() {
        out.push('.');
    }
    out
}

fn saturate(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The key [`serde_saphyr::Spanned`] reads its value through.
const SPANNED_VALUE_KEY: &str = "value";

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Config {
        version: u32,
        bots: Vec<Bot>,
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Bot {
        name: String,
        token: Option<String>,
    }

    #[test]
    fn parse_yaml_accepts_valid_document() {
        let config: Config = parse_yaml("version: 1\nbots:\n  - name: a\n    token: x\n").unwrap();
        assert_eq!(config.bots[0].name, "a");
    }

    #[test]
    fn parse_yaml_unknown_field_has_line_column() {
        let text = "version: 1\nbots:\n  - name: a\n    token: x\n  - name: b\n    tokn: y\n";
        let err = parse_yaml::<Config>(text).unwrap_err();
        assert_eq!(
            err.at,
            Some(Located {
                path: "bots[1].tokn".to_owned(),
                line: 6,
                column: 5,
            }),
            "{err}"
        );
        // The location is in `at`, not repeated in the message, and no
        // source text follows the problem.
        assert_eq!(
            err.message, "unknown field `tokn`, expected one of name, token",
            "{err}"
        );
        assert!(
            err.to_string()
                .starts_with("bots[1].tokn (line 6, column 5): unknown field `tokn`"),
            "{err}"
        );
    }

    #[test]
    fn parse_yaml_duplicate_key_rejected() {
        let text = "version: 1\nversion: 2\nbots: []\n";
        let err = parse_yaml::<Config>(text).unwrap_err();
        assert_eq!(
            err.at,
            Some(Located {
                path: ".".to_owned(),
                line: 2,
                column: 1,
            }),
            "{err}"
        );
        assert!(
            err.message.starts_with("duplicate mapping key: version"),
            "{err}"
        );
    }

    #[test]
    fn parse_yaml_keeps_spans() {
        // The path tracker wraps the deserializer; spans must survive it.
        #[derive(serde::Deserialize)]
        struct Named {
            bots: Vec<Name>,
        }
        #[derive(serde::Deserialize)]
        struct Name {
            name: serde_saphyr::Spanned<String>,
        }

        let named: Named = parse_yaml("bots:\n  - name: a\n  - name: bb\n").unwrap();
        let at = named.bots[1].name.referenced;
        assert_eq!((at.line(), at.column()), (3, 11));
    }

    #[test]
    fn parse_yaml_path_skips_spanned_value() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct Outer {
            inner: serde_saphyr::Spanned<Inner>,
        }
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct Inner {
            items: Vec<serde_saphyr::Spanned<u32>>,
        }

        let err = parse_yaml::<Outer>("inner:\n  items: [1, x]\n").unwrap_err();
        assert_eq!(
            err.at,
            Some(Located {
                path: "inner.items[1]".to_owned(),
                line: 2,
                column: 14,
            }),
            "{err}"
        );
    }

    #[test]
    fn parse_yaml_error_quotes_no_source_text() {
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct Doc {
            url: String,
            token: String,
            version: u32,
        }

        let text = "url: https://user:pa55w0rd@host/x?k=v\n\
                    token: 123456789:AAbbccddeeffgghhiijjkkllmmnnoopp_qq\n\
                    version: nope\n";
        let err = parse_yaml::<Doc>(text).unwrap_err();
        assert_eq!(
            err.at.as_ref().map(|at| (at.path.as_str(), at.line)),
            Some(("version", 3)),
            "{err}"
        );
        let rendered = format!("{err} {err:?}");
        for leaked in ["pa55w0rd", "k=v", "AAbbccddeeffgghhiijjkkllmmnnoopp_qq"] {
            assert!(!rendered.contains(leaked), "{leaked} in {rendered}");
        }
    }

    #[test]
    fn parse_yaml_message_redacts_token_shapes() {
        // A scalar the message quotes passes through token-shape redaction.
        let text = "version: 123456789:AAbbccddeeffgghhiijjkkllmmnnoopp_qq\nbots: []\n";
        let err = parse_yaml::<Config>(text).unwrap_err();
        let rendered = format!("{err} {err:?}");
        assert!(
            !rendered.contains("AAbbccddeeffgghhiijjkkllmmnnoopp_qq"),
            "{rendered}"
        );
    }
}
