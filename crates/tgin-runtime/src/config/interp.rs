//! `${env:NAME}` and `${file:PATH}` substitution.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

/// The process environment, as seen by [`interpolate`]. Tests use [`MapEnv`]
/// instead of mutating the real environment.
pub trait Env: Send + Sync {
    /// The variable's value, or `None` if it is unset or not valid Unicode.
    fn var(&self, name: &str) -> Option<String>;
    /// The whole file as UTF-8.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read or is not UTF-8.
    fn read_file(&self, path: &Path) -> io::Result<String>;
}

/// The real environment and file system.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsEnv;

impl Env for OsEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn read_file(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
}

/// An in-memory environment for tests. `files` is keyed by the path
/// [`interpolate`] reads, that is `base_dir` joined with the reference.
#[derive(Clone, Debug, Default)]
pub struct MapEnv {
    pub vars: HashMap<String, String>,
    pub files: HashMap<PathBuf, String>,
}

impl Env for MapEnv {
    fn var(&self, name: &str) -> Option<String> {
        self.vars.get(name).cloned()
    }

    fn read_file(&self, path: &Path) -> io::Result<String> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InterpError {
    #[error("environment variable {0} is not set")]
    MissingVar(String),
    #[error("cannot read {}: {error}", path.display())]
    UnreadableFile { path: PathBuf, error: String },
    #[error("bad interpolation: {0}")]
    BadSyntax(String),
    #[error("unknown interpolation scheme `{0}`; expected `env` or `file`")]
    UnknownScheme(String),
}

/// Replaces every `${env:NAME}` with the variable's value and every
/// `${file:PATH}` with the file's content, minus one trailing `\n` or
/// `\r\n`. A relative `PATH` is resolved against `base_dir`. `$${` produces
/// a literal `${`; references do not nest, and the substituted values are
/// not scanned again.
///
/// # Errors
///
/// Fails on the first reference that is malformed, uses an unknown scheme,
/// names an unset variable or an unreadable file.
pub fn interpolate(template: &str, env: &dyn Env, base_dir: &Path) -> Result<String, InterpError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let tail = &rest[dollar..];
        if let Some(after) = tail.strip_prefix("$${") {
            out.push_str("${");
            rest = after;
        } else if let Some(after) = tail.strip_prefix("${") {
            let Some(end) = after.find('}') else {
                return Err(InterpError::BadSyntax(format!(
                    "`{}` has no closing `}}`",
                    excerpt(tail)
                )));
            };
            out.push_str(&resolve(&after[..end], env, base_dir)?);
            rest = &after[end + 1..];
        } else {
            out.push('$');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// The value of the reference `${body}`.
fn resolve(body: &str, env: &dyn Env, base_dir: &Path) -> Result<String, InterpError> {
    if body.contains("${") {
        return Err(InterpError::BadSyntax(format!(
            "`${{{body}}}` is nested; references do not nest"
        )));
    }
    let Some((scheme, arg)) = body
        .split_once(':')
        .filter(|(scheme, _)| !scheme.is_empty())
    else {
        return Err(InterpError::BadSyntax(format!(
            "`${{{body}}}` has no scheme; use `${{env:NAME}}` or `${{file:PATH}}`"
        )));
    };
    if !matches!(scheme, "env" | "file") {
        return Err(InterpError::UnknownScheme(scheme.to_owned()));
    }
    if arg.is_empty() {
        return Err(InterpError::BadSyntax(format!(
            "`${{{body}}}` names no {}",
            if scheme == "env" { "variable" } else { "file" }
        )));
    }
    if scheme == "env" {
        return env
            .var(arg)
            .ok_or_else(|| InterpError::MissingVar(arg.to_owned()));
    }
    let path = base_dir.join(arg);
    let mut content = env
        .read_file(&path)
        .map_err(|err| InterpError::UnreadableFile {
            error: err.to_string(),
            path,
        })?;
    let trimmed = content
        .strip_suffix("\r\n")
        .or_else(|| content.strip_suffix('\n'))
        .map(str::len);
    if let Some(len) = trimmed {
        content.truncate(len);
    }
    Ok(content)
}

/// The start of `text`, for error messages about an unterminated reference.
fn excerpt(text: &str) -> &str {
    let end = text
        .char_indices()
        .nth(40)
        .map_or(text.len(), |(index, _)| index);
    let end = text[..end].find('\n').unwrap_or(end);
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> MapEnv {
        let base = Path::new("/etc/tgin");
        MapEnv {
            vars: HashMap::from([("TOKEN".to_owned(), "123:abc".to_owned())]),
            files: HashMap::from([
                (base.join("secret.txt"), "s3cret\n\n".to_owned()),
                (base.join("crlf.txt"), "crlf\r\n".to_owned()),
                (base.join("bare.txt"), "bare".to_owned()),
            ]),
        }
    }

    fn run(template: &str) -> Result<String, InterpError> {
        interpolate(template, &env(), Path::new("/etc/tgin"))
    }

    #[test]
    fn interp_env_file_escape_and_errors() {
        assert_eq!(run("token: ${env:TOKEN}").unwrap(), "token: 123:abc");
        assert_eq!(
            run("a${env:TOKEN}b${env:TOKEN}").unwrap(),
            "a123:abcb123:abc"
        );
        // Exactly one trailing newline is trimmed.
        assert_eq!(run("${file:secret.txt}").unwrap(), "s3cret\n");
        assert_eq!(run("[${file:crlf.txt}]").unwrap(), "[crlf]");
        assert_eq!(run("${file:bare.txt}").unwrap(), "bare");
        // Escapes and lone dollars stay literal.
        assert_eq!(run("$${env:TOKEN}").unwrap(), "${env:TOKEN}");
        assert_eq!(run("cost: $5, $$ and $").unwrap(), "cost: $5, $$ and $");
        // Substituted values are not scanned again.
        let mut nested_value = env();
        nested_value
            .vars
            .insert("REF".to_owned(), "${env:TOKEN}".to_owned());
        assert_eq!(
            interpolate("${env:REF}", &nested_value, Path::new("/etc/tgin")).unwrap(),
            "${env:TOKEN}"
        );

        assert_eq!(
            run("${env:NOPE}"),
            Err(InterpError::MissingVar("NOPE".to_owned()))
        );
        assert!(matches!(
            run("${file:missing.txt}"),
            Err(InterpError::UnreadableFile { path, .. })
                if path == Path::new("/etc/tgin").join("missing.txt")
        ));
        for bad in [
            "${env:TOKEN",
            "${env:${env:TOKEN}}",
            "${env:}",
            "${file:}",
            "${TOKEN}",
            "${:TOKEN}",
        ] {
            assert!(
                matches!(run(bad), Err(InterpError::BadSyntax(_))),
                "{bad:?} gave {:?}",
                run(bad)
            );
        }
        assert_eq!(
            run("${vault:kv/token}"),
            Err(InterpError::UnknownScheme("vault".to_owned()))
        );
    }
}
