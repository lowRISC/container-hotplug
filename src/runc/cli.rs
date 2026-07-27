//! As much of `runc`'s command line as we need to understand.
//!
//! Only the options we act on are declared. Unrecognised subcommands are captured by
//! [`Subcommand::Other`] and the original arguments are handed to `runc` verbatim, so we neither
//! need to model nor to reconstruct them.

use std::path::PathBuf;
use std::sync::LazyLock;

use clap::ValueEnum;

/// Format of `runc`'s log output, from `--log-format`.
#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Line-delimited logrus-style JSON. See [`crate::runc::log::JsonLogger`].
    Json,
    Text,
}

/// runc command line.
#[derive(clap::Parser)]
pub struct Command {
    #[command(flatten)]
    pub global: GlobalOptions,

    #[command(subcommand)]
    pub command: Subcommand,
}

/// Options accepted before the subcommand.
#[derive(clap::Args)]
pub struct GlobalOptions {
    #[arg(long)]
    pub debug: bool,

    /// File to append log messages to, instead of stderr.
    #[arg(long)]
    pub log: Option<PathBuf>,

    #[arg(long, default_value = "text")]
    pub log_format: LogFormat,

    /// Directory holding `runc`'s per-container state, where we look for `state.json`.
    #[arg(long, default_value = "/run/runc")]
    pub root: PathBuf,

    #[arg(long)]
    pub systemd_cgroup: bool,
}

/// The verb `runc` was invoked with.
#[derive(clap::Subcommand)]
pub enum Subcommand {
    // We only care about the `create` subcommand.
    // We need to be able to parse the rest (hence `trailing_var_arg` and `external_subcommand`) without error, but
    // we don't make use of these and forward to runc directly.
    /// The one verb we add behaviour to; see [`crate::create`].
    Create(CreateOptions),
    /// Shorthand for `create` followed by `start`. Rejected: it gives us no point at which to start
    /// the hotplug daemon before the entrypoint runs, and container managers do not use it.
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Any other verb, forwarded to `runc` unchanged. The captured arguments are unused; we re-exec
    /// with the original [`std::env::args`].
    #[command(external_subcommand)]
    #[allow(unused)]
    Other(Vec<String>),
}

static BUNDLE_DEFAULT: LazyLock<PathBuf> = LazyLock::new(|| std::env::current_dir().unwrap());

/// Options of the `create` subcommand.
#[derive(clap::Args)]
pub struct CreateOptions {
    /// The OCI bundle directory, which is where we read `config.json` and its hotplug annotations
    /// from. Defaults to the current directory, as `runc` does.
    #[arg(short, long, default_value = BUNDLE_DEFAULT.as_os_str())]
    pub bundle: PathBuf,

    #[arg(long)]
    pub console_socket: Option<PathBuf>,

    #[arg(long)]
    pub pid_file: Option<PathBuf>,

    pub container_id: String,
}
