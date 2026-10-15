//! The command line: `tgin [run]`, `tgin config validate|schema|print` and
//! `tgin version`.

use std::borrow::Cow;
use std::ffi::OsString;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use clap::{Args, Parser, Subcommand};
use tgin_core::config::{self, Config, LogLevel};
use tgin_runtime::config::{ConfigError, OsEnv};
use tgin_runtime::lifecycle::Signals;
use tgin_runtime::obs::{LogFormat, LogOptions, init_tracing};
use tgin_runtime::secret::Redactor;

use crate::app::{self, App};
use crate::exit;

/// The tgin version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The commit tgin was built from, when the build set a non-empty
/// `TGIN_COMMIT`.
pub fn commit() -> Option<&'static str> {
    option_env!("TGIN_COMMIT").filter(|commit| !commit.is_empty())
}

/// How long the runtime may take to wind down its tasks after a run.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);

#[derive(Debug, Parser)]
#[command(
    name = "tgin",
    version,
    about = "Routes Telegram Bot API updates to downstream bots",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// The 0.2 config flag; tgin 1.0 explains how to migrate instead.
    #[arg(short = 'f', long = "file", hide = true, value_name = "PATH")]
    legacy_file: Option<PathBuf>,
    #[command(flatten)]
    run: RunArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// The config file.
    #[arg(
        short = 'c',
        long = "config",
        env = "TGIN_CONFIG",
        default_value = "tgin.yaml",
        value_name = "PATH"
    )]
    config: PathBuf,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run tgin (the default).
    Run(RunArgs),
    /// Check, print or describe config files.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Print the version.
    Version,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Check a config file, listing every error and warning.
    Validate { path: PathBuf },
    /// Print the JSON Schema of the config file.
    Schema,
    /// Print a config file with defaults filled in and secrets redacted.
    Print { path: PathBuf },
}

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    Run(PathBuf),
    Validate(PathBuf),
    Schema,
    Print(PathBuf),
    Version,
    /// The 0.2 `-f/--file` flag with its value.
    LegacyFile(PathBuf),
}

/// Parses the command line; `args` starts with the program name.
///
/// # Errors
///
/// clap's error for a malformed command line, `--help` or `--version`.
pub fn parse<I, T>(args: I) -> Result<Invocation, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::try_parse_from(args)?;
    if let Some(file) = cli.legacy_file {
        return Ok(Invocation::LegacyFile(file));
    }
    Ok(match cli.command {
        None => Invocation::Run(cli.run.config),
        Some(Command::Run(run)) => Invocation::Run(run.config),
        Some(Command::Config(ConfigCommand::Validate { path })) => Invocation::Validate(path),
        Some(Command::Config(ConfigCommand::Schema)) => Invocation::Schema,
        Some(Command::Config(ConfigCommand::Print { path })) => Invocation::Print(path),
        Some(Command::Version) => Invocation::Version,
    })
}

/// Runs the command line `args` and returns the process exit code.
pub fn main<I, T>(args: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let invocation = match parse(args) {
        Ok(invocation) => invocation,
        Err(err) => {
            // Help and version go to stdout and succeed; anything else is a
            // usage error.
            let _ = err.print();
            return if err.use_stderr() {
                exit::USAGE
            } else {
                exit::OK
            };
        }
    };
    match invocation {
        Invocation::Run(path) => run(&path),
        Invocation::Validate(path) => validate(&path),
        Invocation::Schema => {
            println!("{}", config::schema());
            exit::OK
        }
        Invocation::Print(path) => print(&path),
        Invocation::Version => {
            println!("{}", version());
            exit::OK
        }
        Invocation::LegacyFile(file) => {
            eprintln!(
                "tgin 1.0 reads YAML; convert with: tgin config migrate {} -o tgin.yaml",
                file.display()
            );
            exit::FAILURE
        }
    }
}

/// `tgin <version>`, with the commit when known.
pub fn version() -> String {
    match commit() {
        Some(commit) => format!("tgin {VERSION} ({commit})"),
        None => format!("tgin {VERSION}"),
    }
}

/// Why a config file could not be loaded.
#[derive(Debug)]
pub enum LoadError {
    Read(io::Error),
    Invalid(Vec<ConfigError>),
}

/// Reads and loads the config file at `path`, with secrets from the process
/// environment and files.
///
/// # Errors
///
/// The file cannot be read, or its errors.
pub fn load(path: &Path) -> Result<Config, LoadError> {
    let text = std::fs::read_to_string(path).map_err(LoadError::Read)?;
    config::load(&text, path, &OsEnv).map_err(LoadError::Invalid)
}

/// Prints why `path` did not load to stderr. Config errors carry no secret.
fn report(path: &Path, err: &LoadError) {
    match err {
        LoadError::Read(err) => {
            eprintln!(
                "tgin: cannot read the config file {}: {err}",
                path.display()
            );
        }
        LoadError::Invalid(errors) => {
            for err in errors {
                eprintln!("error: {err}");
            }
            eprintln!(
                "tgin: {} has {} error{}",
                path.display(),
                errors.len(),
                if errors.len() == 1 { "" } else { "s" }
            );
        }
    }
}

fn validate(path: &Path) -> u8 {
    match load(path) {
        Ok(cfg) => {
            for warning in &cfg.warnings {
                eprintln!("{warning}");
            }
            println!("{}: valid", path.display());
            exit::OK
        }
        Err(err) => {
            report(path, &err);
            exit::FAILURE
        }
    }
}

fn print(path: &Path) -> u8 {
    match load(path) {
        Ok(cfg) => {
            for warning in &cfg.warnings {
                eprintln!("{warning}");
            }
            match serde_json::to_string_pretty(&config::redacted_json(&cfg)) {
                Ok(json) => {
                    println!("{json}");
                    exit::OK
                }
                Err(err) => {
                    eprintln!("tgin: rendering the config failed: {err}");
                    exit::FAILURE
                }
            }
        }
        Err(err) => {
            report(path, &err);
            exit::FAILURE
        }
    }
}

/// `tgin run`: loads the config, starts logging and the runtime, runs the
/// app until it shuts down and returns its exit code.
fn run(path: &Path) -> u8 {
    let cfg = match load(path) {
        Ok(cfg) => cfg,
        Err(err) => {
            report(path, &err);
            return exit::FAILURE;
        }
    };

    let redactor = Arc::new(ArcSwap::from_pointee(Redactor::new(app::secrets(&cfg))));
    let log = LogOptions {
        level: level_name(cfg.log.level).to_owned(),
        format: match cfg.log.format {
            config::LogFormat::Text => LogFormat::Text,
            config::LogFormat::Json => LogFormat::Json,
        },
    };
    if let Err(err) = init_tracing(&log, redacting(redactor)) {
        eprintln!("tgin: {err}");
        return exit::FAILURE;
    }
    install_panic_hook();
    for warning in &cfg.warnings {
        tracing::warn!("{warning}");
    }

    let threads = cfg
        .runtime
        .worker_threads
        .unwrap_or_else(|| std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN));
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads.get())
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!(error = %err, "starting the async runtime failed");
            return exit::FAILURE;
        }
    };
    let code = runtime.block_on(async move {
        let signals = match Signals::install() {
            Ok(signals) => signals,
            Err(err) => {
                tracing::error!(error = %err, "no shutdown signal could be registered");
                return exit::FAILURE;
            }
        };
        for (kind, err) in signals.failed() {
            tracing::warn!(signal = ?kind, error = %err, "a signal listener could not be registered; that signal is not handled");
        }
        match App::start(cfg).await {
            Ok(app) => app.run(signals).await.code(),
            Err(err) => {
                tracing::error!(error = %err, "tgin could not start");
                exit::FAILURE
            }
        }
    });
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
    code
}

/// Masks log lines with the current redactor. It sits behind an `ArcSwap`
/// so that a reload can swap in its config's secrets.
fn redacting(
    redactor: Arc<ArcSwap<Redactor>>,
) -> impl Fn(&str) -> Cow<'_, str> + Send + Sync + 'static {
    move |line| redactor.load().redact(line)
}

fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "trace",
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

/// Logs every panic through `tracing`, whose writer masks secrets, before
/// the default hook runs.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "panicked");
        default(info);
    }));
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use clap::error::ErrorKind;

    use super::*;

    fn parsed(args: &[&str]) -> Invocation {
        parse(std::iter::once("tgin").chain(args.iter().copied())).expect("parses")
    }

    fn error(args: &[&str]) -> ErrorKind {
        parse(std::iter::once("tgin").chain(args.iter().copied()))
            .expect_err("rejected")
            .kind()
    }

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_takes_config_at_top_level_or_after_run() {
        assert_eq!(parsed(&["-c", "a.yaml"]), Invocation::Run("a.yaml".into()));
        assert_eq!(
            parsed(&["--config", "a.yaml"]),
            Invocation::Run("a.yaml".into())
        );
        assert_eq!(
            parsed(&["run", "-c", "b.yaml"]),
            Invocation::Run("b.yaml".into())
        );
    }

    #[test]
    fn config_and_version_commands() {
        assert_eq!(
            parsed(&["config", "validate", "v.yaml"]),
            Invocation::Validate("v.yaml".into())
        );
        assert_eq!(parsed(&["config", "schema"]), Invocation::Schema);
        assert_eq!(
            parsed(&["config", "print", "p.yaml"]),
            Invocation::Print("p.yaml".into())
        );
        assert_eq!(parsed(&["version"]), Invocation::Version);
    }

    #[test]
    fn legacy_file_flag_is_recognized() {
        assert_eq!(
            parsed(&["-f", "tgin.ron"]),
            Invocation::LegacyFile("tgin.ron".into())
        );
        assert_eq!(
            parsed(&["--file=old.ron"]),
            Invocation::LegacyFile("old.ron".into())
        );
    }

    #[test]
    fn malformed_command_lines_are_errors() {
        assert_eq!(error(&["--bogus"]), ErrorKind::UnknownArgument);
        assert_eq!(error(&["frobnicate"]), ErrorKind::InvalidSubcommand);
        assert_eq!(
            error(&["-c", "x.yaml", "config", "schema"]),
            ErrorKind::ArgumentConflict
        );
        assert_eq!(
            error(&["config"]),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert_eq!(error(&["--version"]), ErrorKind::DisplayVersion);
        assert_eq!(error(&["--help"]), ErrorKind::DisplayHelp);
    }

    #[test]
    fn version_names_tgin() {
        assert!(version().starts_with(&format!("tgin {VERSION}")));
    }

    #[test]
    fn level_names_match_the_log_levels() {
        for (level, name) in [
            (LogLevel::Trace, "trace"),
            (LogLevel::Debug, "debug"),
            (LogLevel::Info, "info"),
            (LogLevel::Warn, "warn"),
            (LogLevel::Error, "error"),
        ] {
            assert_eq!(level_name(level), name);
        }
    }
}
