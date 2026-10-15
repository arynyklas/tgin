//! YAML deserialization with located errors.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde_path_to_error::Segment;
use serde_saphyr::{DuplicateKeyPolicy, Localizer, Location, SnippetMode, UserMessageFormatter};

use super::{ConfigError, Located};

/// Deserializes `text`, rejecting duplicate keys. On failure the error
/// carries the YAML path, line and column of the offending node, and its
/// message is the problem followed by a source snippet headed by `origin`.
///
/// # Errors
///
/// Fails if `text` is not valid YAML or does not match `T`.
pub fn parse_yaml<T: DeserializeOwned>(text: &str, origin: &Path) -> Result<T, ConfigError> {
    let options = serde_saphyr::options! {
        duplicate_keys: DuplicateKeyPolicy::Error,
        with_snippet: true,
    };
    let mut failed_at = None;
    serde_saphyr::with_deserializer_from_str_with_options(text, options, |de| {
        let mut track = serde_path_to_error::Track::new();
        T::deserialize(serde_path_to_error::Deserializer::new(de, &mut track)).inspect_err(|_| {
            failed_at = Some(track.path());
        })
    })
    .map_err(|err| ConfigError {
        at: err.location().map(|location| Located {
            path: failed_at.as_ref().map_or_else(|| ".".to_owned(), yaml_path),
            line: saturate(location.line()),
            column: saturate(location.column()),
        }),
        message: message(&err, origin),
        hint: None,
    })
}

/// The problem, without a location since [`Located`] carries it, then the
/// source snippet if there is one.
fn message(err: &serde_saphyr::Error, origin: &Path) -> String {
    let formatter = UserMessageFormatter.with_localizer(&NoLocationSuffix);
    let mut message = err.render_with_options(serde_saphyr::render_options! {
        formatter: &formatter,
        snippets: SnippetMode::Off,
    });
    let origin = origin.display().to_string();
    let rendered = err.render_with_options(serde_saphyr::render_options! {
        formatter: &formatter,
        source_name: Some(&origin),
    });
    // A snippet's first line is a title repeating the location and problem.
    if let Some((_title, snippet)) = rendered.split_once('\n') {
        message.push('\n');
        message.push_str(snippet.trim_end());
    }
    message
}

/// Renders messages without serde-saphyr's `at line L, column C` suffix.
struct NoLocationSuffix;

impl Localizer for NoLocationSuffix {
    fn attach_location<'a>(&self, base: Cow<'a, str>, _location: Location) -> Cow<'a, str> {
        base
    }
}

/// `path` as `bots[1].token`; `.` is the document root. Segments the tracker
/// could not name (such as a duplicate key) are left out.
fn yaml_path(path: &serde_path_to_error::Path) -> String {
    let mut out = String::new();
    for segment in path {
        match segment {
            Segment::Seq { index } => {
                let _ = write!(out, "[{index}]");
            }
            Segment::Map { key: name } | Segment::Enum { variant: name } => {
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
        let config: Config = parse_yaml(
            "version: 1\nbots:\n  - name: a\n    token: x\n",
            Path::new("tgin.yaml"),
        )
        .unwrap();
        assert_eq!(config.bots[0].name, "a");
    }

    #[test]
    fn parse_yaml_unknown_field_has_line_column() {
        let text = "version: 1\nbots:\n  - name: a\n    token: x\n  - name: b\n    tokn: y\n";
        let err = parse_yaml::<Config>(text, Path::new("conf/tgin.yaml")).unwrap_err();
        assert_eq!(
            err.at,
            Some(Located {
                path: "bots[1].tokn".to_owned(),
                line: 6,
                column: 5,
            }),
            "{err}"
        );
        // The location is in `at`, not repeated in the message; the snippet
        // follows the problem.
        assert!(
            err.message
                .starts_with("unknown field `tokn`, expected one of name, token\n"),
            "{err}"
        );
        assert!(
            err.message.contains("conf/tgin.yaml:6:5"),
            "no snippet: {err}"
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
        let err = parse_yaml::<Config>(text, Path::new("tgin.yaml")).unwrap_err();
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

        let named: Named =
            parse_yaml("bots:\n  - name: a\n  - name: bb\n", Path::new("tgin.yaml")).unwrap();
        let at = named.bots[1].name.referenced;
        assert_eq!((at.line(), at.column()), (3, 11));
    }
}
