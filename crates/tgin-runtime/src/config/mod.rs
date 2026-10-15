//! Configuration toolkit: secret interpolation, YAML loading with located
//! errors, JSON Schema export and file watching.

mod interp;
mod watch;
mod yaml;

use std::fmt;

pub use self::interp::{Env, InterpError, MapEnv, OsEnv, interpolate};
pub use self::watch::{ConfigWatcher, watch_config};
pub use self::yaml::parse_yaml;

/// Where in a configuration document an error was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    /// YAML path to the offending node, such as `bots[1].token`.
    pub path: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
}

/// Whether a [`ConfigError`] rejects the configuration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Severity {
    /// The configuration is rejected.
    #[default]
    Error,
    /// The configuration loads, but probably does not do what was meant.
    Warning,
}

/// A configuration error or warning, displayed as
/// `<path> (line L, column C): message` followed by `\n  hint: …` when there
/// is a hint. Without a location only the message (and hint) is shown, and
/// a warning starts with `warning: `.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub severity: Severity,
    pub at: Option<Located>,
    pub message: String,
    pub hint: Option<String>,
}

impl ConfigError {
    /// An error with no location and no hint.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            at: None,
            message: message.into(),
            hint: None,
        }
    }

    /// A warning with no location and no hint.
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::new(message)
        }
    }

    /// Sets the location.
    #[must_use]
    pub fn at(mut self, at: Located) -> Self {
        self.at = Some(at);
        self
    }

    /// Sets the hint.
    #[must_use]
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.severity == Severity::Warning {
            f.write_str("warning: ")?;
        }
        if let Some(at) = &self.at {
            write!(f, "{} (line {}, column {}): ", at.path, at.line, at.column)?;
        }
        f.write_str(&self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, "\n  hint: {hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigError {}

/// The JSON Schema (draft 2020-12) of `T`, pretty-printed.
pub fn schema_json<T: schemars::JsonSchema>() -> String {
    format!("{:#}", schemars::schema_for!(T).as_value())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_error_display() {
        let located = ConfigError::new("unknown field `tokn`")
            .at(Located {
                path: "bots[1].tokn".to_owned(),
                line: 7,
                column: 5,
            })
            .hint("did you mean `token`?");
        assert_eq!(
            located.to_string(),
            "bots[1].tokn (line 7, column 5): unknown field `tokn`\n  hint: did you mean `token`?"
        );
        assert_eq!(ConfigError::new("no bots").to_string(), "no bots");
        assert_eq!(
            ConfigError::warning("no routes").to_string(),
            "warning: no routes"
        );
    }

    #[test]
    fn schema_json_is_2020_12() {
        #[derive(schemars::JsonSchema)]
        #[allow(dead_code)]
        struct Sample {
            name: String,
        }

        let schema = schema_json::<Sample>();
        assert!(
            schema.contains(r#""$schema": "https://json-schema.org/draft/2020-12/schema""#),
            "{schema}"
        );
        assert!(
            schema.contains("\n  \"properties\""),
            "not pretty: {schema}"
        );
    }
}
