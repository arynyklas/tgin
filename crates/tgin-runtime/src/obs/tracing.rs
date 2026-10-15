//! The process-wide `tracing` subscriber: a reloadable level filter and a
//! text or JSON formatter that writes to stdout through a redactor.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::io::{self, IsTerminal as _};
use std::mem;

use tracing::Subscriber;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::fmt::{self, MakeWriter};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::{SubscriberInitExt as _, TryInitError};
use tracing_subscriber::{Layer as _, Registry, reload};

/// Log output settings.
#[derive(Clone, Debug)]
pub struct LogOptions {
    /// Maximum level: `off`, `error`, `warn`, `info`, `debug` or `trace`.
    pub level: String,
    pub format: LogFormat,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            format: LogFormat::default(),
        }
    }
}

/// How each event is rendered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// One compact line, ANSI-coloured when [`use_ansi`] allows it.
    #[default]
    Text,
    /// One JSON object per line, never coloured.
    Json,
}

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("invalid log level {0:?}; expected off, error, warn, info, debug or trace")]
    InvalidLevel(String),
    #[error("installing the tracing subscriber failed: {0}")]
    Install(#[from] TryInitError),
    #[error("reloading the log level failed: {0}")]
    Reload(#[from] reload::Error),
}

/// Changes the level of the installed subscriber at runtime.
#[derive(Clone, Debug)]
pub struct LogHandle {
    filter: reload::Handle<EnvFilter, Registry>,
    /// `RUST_LOG` as read at startup; it keeps overriding the level.
    rust_log: Option<String>,
}

impl LogHandle {
    /// Replaces the level filter. When `RUST_LOG` was set at startup, its
    /// directives still take precedence over `level`.
    ///
    /// # Errors
    ///
    /// Fails if `level` is not a level name, or the subscriber is gone.
    pub fn set_level(&self, level: &str) -> Result<(), LogError> {
        self.filter
            .reload(filter(level, self.rust_log.as_deref())?)?;
        Ok(())
    }
}

/// Installs the global subscriber, writing to stdout. Every formatted line
/// passes through `redact` before it is written, so `redact` sees whole
/// lines and can be backed by a secret set that changes at runtime.
///
/// ANSI colours are used only for [`LogFormat::Text`], and only as
/// [`use_ansi`] decides from stdout and `NO_COLOR`. A non-empty `RUST_LOG`
/// overrides `opts.level`.
///
/// # Errors
///
/// Fails if `opts.level` is not a level name or a global subscriber is
/// already installed.
pub fn init_tracing(
    opts: &LogOptions,
    redact: impl Fn(&str) -> Cow<'_, str> + Send + Sync + 'static,
) -> Result<LogHandle, LogError> {
    let no_color = std::env::var_os("NO_COLOR");
    let ansi = use_ansi(
        io::stdout().is_terminal(),
        no_color.as_deref().map(OsStr::to_string_lossy).as_deref(),
    );
    let rust_log = std::env::var("RUST_LOG").ok();
    let (subscriber, handle) = subscriber(opts, rust_log.as_deref(), ansi, io::stdout, redact)?;
    subscriber.try_init()?;
    Ok(handle)
}

/// Whether text logs get ANSI colours: only when stdout is a terminal
/// (`docker logs` and log files get plain text) and `NO_COLOR` is unset or
/// empty (<https://no-color.org>). Decided here because `with_ansi`
/// overrides tracing-subscriber's own `NO_COLOR` check, which never looks at
/// the terminal.
pub fn use_ansi(is_terminal: bool, no_color: Option<&str>) -> bool {
    is_terminal && no_color.is_none_or(str::is_empty)
}

/// Builds the subscriber [`init_tracing`] installs, without installing it.
fn subscriber<W, F>(
    opts: &LogOptions,
    rust_log: Option<&str>,
    ansi: bool,
    writer: W,
    redact: F,
) -> Result<(impl Subscriber + Send + Sync + 'static, LogHandle), LogError>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
    F: Fn(&str) -> Cow<'_, str> + Send + Sync + 'static,
{
    let (filter, filter_handle) = reload::Layer::new(filter(&opts.level, rust_log)?);
    let writer = RedactingMakeWriter {
        inner: writer,
        redact,
    };
    let output = match opts.format {
        LogFormat::Text => fmt::layer()
            .compact()
            .with_ansi(ansi)
            .with_writer(writer)
            .boxed(),
        LogFormat::Json => fmt::layer()
            .json()
            .with_ansi(false)
            .with_writer(writer)
            .boxed(),
    };
    let handle = LogHandle {
        filter: filter_handle,
        rust_log: rust_log.map(str::to_owned),
    };
    Ok((
        tracing_subscriber::registry().with(filter).with(output),
        handle,
    ))
}

/// `level` as the default directive, overridden by any `RUST_LOG` directive.
fn filter(level: &str, rust_log: Option<&str>) -> Result<EnvFilter, LogError> {
    let level: LevelFilter = level
        .parse()
        .map_err(|_| LogError::InvalidLevel(level.to_owned()))?;
    Ok(EnvFilter::builder()
        .with_default_directive(level.into())
        .parse_lossy(rust_log.unwrap_or_default()))
}

/// Hands out [`RedactingWriter`]s over `inner`'s writers.
struct RedactingMakeWriter<M, F> {
    inner: M,
    redact: F,
}

impl<'a, M, F> MakeWriter<'a> for RedactingMakeWriter<M, F>
where
    M: MakeWriter<'a>,
    F: Fn(&str) -> Cow<'_, str> + 'a,
{
    type Writer = RedactingWriter<'a, M::Writer, F>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.make_writer(),
            redact: &self.redact,
            pending: Vec::new(),
        }
    }
}

/// Redacts whole lines on their way to `inner`. A line written in several
/// pieces is held until its newline, or until the writer is dropped, so a
/// secret split across writes is still masked. The formatter writes each
/// event as one line through a fresh writer.
struct RedactingWriter<'a, W, F>
where
    W: io::Write,
    F: Fn(&str) -> Cow<'_, str>,
{
    inner: W,
    redact: &'a F,
    /// The unfinished last line.
    pending: Vec<u8>,
}

impl<W, F> io::Write for RedactingWriter<'_, W, F>
where
    W: io::Write,
    F: Fn(&str) -> Cow<'_, str>,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(newline) = buf.iter().rposition(|&b| b == b'\n') else {
            self.pending.extend_from_slice(buf);
            return Ok(buf.len());
        };
        let (lines, rest) = buf.split_at(newline + 1);
        if self.pending.is_empty() {
            emit(&mut self.inner, self.redact, lines)?;
        } else {
            let mut pending = mem::take(&mut self.pending);
            pending.extend_from_slice(lines);
            emit(&mut self.inner, self.redact, &pending)?;
        }
        self.pending.extend_from_slice(rest);
        Ok(buf.len())
    }

    /// Flushes `inner`; an unfinished line stays held.
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W, F> Drop for RedactingWriter<'_, W, F>
where
    W: io::Write,
    F: Fn(&str) -> Cow<'_, str>,
{
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            // Nowhere to report a failed log write.
            let _ = emit(&mut self.inner, self.redact, &self.pending);
        }
    }
}

fn emit<F>(inner: &mut impl io::Write, redact: &F, bytes: &[u8]) -> io::Result<()>
where
    F: Fn(&str) -> Cow<'_, str>,
{
    let text = String::from_utf8_lossy(bytes);
    inner.write_all(redact(&text).as_bytes())
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write as _};
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use parking_lot::Mutex;
    use tracing::{debug, info, warn};
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    /// Collects everything the subscriber writes.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn lines(&self) -> Vec<String> {
            String::from_utf8(self.0.lock().clone())
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }

    fn text(level: &str) -> LogOptions {
        LogOptions {
            level: level.to_owned(),
            format: LogFormat::Text,
        }
    }

    fn no_redaction(line: &str) -> Cow<'_, str> {
        Cow::Borrowed(line)
    }

    #[test]
    fn use_ansi_table() {
        assert!(use_ansi(true, None));
        assert!(use_ansi(true, Some("")));
        assert!(!use_ansi(true, Some("1")));
        assert!(!use_ansi(false, None));
        assert!(!use_ansi(false, Some("")));
        assert!(!use_ansi(false, Some("1")));
    }

    #[test]
    fn redacting_writer_masks_secret_in_formatted_line() {
        let secrets = Arc::new(ArcSwap::from_pointee(vec!["first-secret".to_owned()]));
        let swap = Arc::clone(&secrets);
        let capture = Capture::default();
        let (subscriber, _handle) =
            subscriber(&text("info"), None, false, capture.clone(), move |line| {
                let mut out = Cow::Borrowed(line);
                for secret in swap.load().iter() {
                    if out.contains(secret.as_str()) {
                        out = Cow::Owned(out.replace(secret.as_str(), "#####"));
                    }
                }
                out
            })
            .unwrap();

        tracing::subscriber::with_default(subscriber, || {
            info!(
                url = "https://x/botfirst-secret/a",
                "a=first-secret b=second-secret"
            );
            secrets.store(Arc::new(vec!["second-secret".to_owned()]));
            info!("a=first-secret b=second-secret");
        });

        let lines = capture.lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(!lines[0].contains("first-secret"), "{}", lines[0]);
        assert!(lines[0].contains("a=##### b=second-secret"), "{}", lines[0]);
        assert!(lines[0].contains("https://x/bot#####/a"), "{}", lines[0]);
        assert!(!lines[1].contains("second-secret"), "{}", lines[1]);
        assert!(lines[1].contains("a=first-secret b=#####"), "{}", lines[1]);
    }

    #[test]
    fn redacting_writer_holds_partial_line_until_newline() {
        fn mask_token(line: &str) -> Cow<'_, str> {
            Cow::Owned(line.replace("token", "#####"))
        }

        let capture = Capture::default();
        let make = RedactingMakeWriter {
            inner: capture.clone(),
            redact: mask_token,
        };

        let mut writer = make.make_writer();
        writer.write_all(b"one tok").unwrap();
        writer.write_all(b"en\ntwo to").unwrap();
        assert_eq!(capture.lines(), ["one #####"]);
        writer.write_all(b"ken").unwrap();
        drop(writer);

        assert_eq!(capture.lines(), ["one #####", "two #####"]);
    }

    #[test]
    fn log_level_reload() {
        let capture = Capture::default();
        let (subscriber, handle) =
            subscriber(&text("info"), None, false, capture.clone(), no_redaction).unwrap();

        tracing::subscriber::with_default(subscriber, || {
            debug!("before reload");
            info!("info before reload");
            handle.set_level("debug").unwrap();
            debug!("after reload");
        });

        let lines = capture.lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("info before reload"), "{}", lines[0]);
        assert!(lines[1].contains("after reload"), "{}", lines[1]);

        assert!(matches!(
            handle.set_level("verbose"),
            Err(LogError::InvalidLevel(level)) if level == "verbose"
        ));
    }

    #[test]
    fn rust_log_overrides_level() {
        let capture = Capture::default();
        let (subscriber, _handle) = subscriber(
            &text("debug"),
            Some("warn"),
            false,
            capture.clone(),
            no_redaction,
        )
        .unwrap();

        tracing::subscriber::with_default(subscriber, || {
            info!("dropped");
            warn!("kept");
        });

        let lines = capture.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("kept"), "{}", lines[0]);
    }

    #[test]
    fn invalid_level_is_rejected() {
        let result = subscriber(
            &text("verbose"),
            None,
            false,
            Capture::default(),
            no_redaction,
        );
        assert!(matches!(result, Err(LogError::InvalidLevel(level)) if level == "verbose"));
    }

    #[test]
    fn json_format_redacts() {
        let capture = Capture::default();
        let options = LogOptions {
            level: "info".to_owned(),
            format: LogFormat::Json,
        };
        let (subscriber, _handle) = subscriber(&options, None, true, capture.clone(), |line| {
            Cow::Owned(line.replace("hunter2", "#####"))
        })
        .unwrap();

        tracing::subscriber::with_default(subscriber, || info!(password = "hunter2", "login"));

        let lines = capture.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with('{'), "{}", lines[0]);
        assert!(lines[0].contains("\"password\":\"#####\""), "{}", lines[0]);
        assert!(!lines[0].contains('\u{1b}'), "ANSI in JSON: {}", lines[0]);
    }
}
