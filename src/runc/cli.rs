use std::path::PathBuf;
use std::sync::LazyLock;

use clap::ValueEnum;

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
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

#[derive(clap::Args)]
pub struct GlobalOptions {
    #[arg(long)]
    pub debug: bool,

    #[arg(long)]
    pub log: Option<PathBuf>,

    #[arg(long, default_value = "text")]
    pub log_format: LogFormat,

    #[arg(long, default_value = "/run/runc")]
    pub root: PathBuf,

    #[arg(long)]
    pub systemd_cgroup: bool,
}

#[derive(clap::Subcommand)]
pub enum Subcommand {
    // We only care about the `create` subcommand.
    // We need to be able to parse the rest (hence `trailing_var_arg` and `external_subcommand`) without error, but
    // we don't make use of these and forward to runc directly.
    Create(CreateOptions),
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(external_subcommand)]
    #[allow(unused)]
    Other(Vec<String>),
}

static BUNDLE_DEFAULT: LazyLock<PathBuf> = LazyLock::new(|| std::env::current_dir().unwrap());

#[derive(clap::Args)]
pub struct CreateOptions {
    #[arg(short, long, default_value = BUNDLE_DEFAULT.as_os_str())]
    pub bundle: PathBuf,

    #[arg(long)]
    pub console_socket: Option<PathBuf>,

    #[arg(long)]
    pub pid_file: Option<PathBuf>,

    pub container_id: String,
}
