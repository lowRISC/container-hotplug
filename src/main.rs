mod cgroup;
mod cli;
mod dev;
mod hotplug;
mod runc;
mod util;

use cli::{DeviceRef, Symlink};
use hotplug::{AttachedDevice, HotPlug};

use std::fmt::Display;
use std::fs::File;
use std::io::{PipeWriter, Read};
use std::mem::ManuallyDrop;
use std::os::unix::process::CommandExt;
use std::pin::pin;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use log::info;
use runc::Container;
use runc::cli::{CreateOptions, GlobalOptions};
use rustix::process::Signal;
use tokio_stream::StreamExt;

#[derive(Clone)]
enum Event {
    Attach(AttachedDevice),
    Detach(AttachedDevice),
    Initialized,
    Stopped,
}

impl Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::Attach(dev) => {
                write!(f, "Attaching device {dev}")
            }
            Event::Detach(dev) => {
                write!(f, "Detaching device {dev}")
            }
            Event::Initialized => {
                write!(f, "Container initialized")
            }
            Event::Stopped => {
                write!(f, "Container stopped")
            }
        }
    }
}

async fn create(global: GlobalOptions, create: CreateOptions, notifier: PipeWriter) -> Result<()> {
    let mut notifier = Some(notifier);

    let config = runc::config::Config::from_bundle(&create.bundle)?;
    let mut devices = Vec::new();
    let device_annotation = config
        .annotations
        .get("org.lowrisc.hotplug.devices")
        .or_else(|| config.annotations.get("org.lowrisc.hotplug.device"))
        .context(
            "Cannot find annotation `org.lowrisc.hotplug.devices`. Please use normal runc instead.",
        )?;
    for device in device_annotation.split(',') {
        let devref: DeviceRef = device.parse()?;
        devices.push(devref.device()?.syspath().to_owned());
    }

    let mut symlinks = Vec::<Symlink>::new();
    if let Some(symlink_annotation) = config.annotations.get("org.lowrisc.hotplug.symlinks") {
        for symlink in symlink_annotation.split(',') {
            symlinks.push(symlink.parse()?);
        }
    }

    // Switch the logger to syslog. The runc logs are barely forwarded to the user or syslog by
    // container managers and orchestrators, while we do want to preserve the hotplug events.
    util::log::global_replace(Box::new(util::log::SyslogLogger::new()?));

    // Delegate to runc to create container.
    let runc = std::process::Command::new("runc")
        .args(std::env::args().skip(1))
        .spawn()
        .context("Cannot start runc")?
        .wait()
        .context("Failed waiting for runc")?;
    if !runc.success() {
        std::process::exit(runc.code().unwrap_or(1));
    }

    let state: runc::state::State =
        runc::state::State::from_root_and_id(&global.root, &create.container_id)?;

    // Create a container handler.
    // To avoid race where the container is deleted before the daemon is started, do this
    // before forking.
    let container = Arc::new(Container::new(&config, &state)?);
    // Prevent the container's destructor from being executed in abnormal exit.
    let container_keep = ManuallyDrop::new(container.clone());

    // Before the parent process returns, we need to ensure that all stdio are redirected, otherwise it can cause
    // issue to the spawning process since it will be unable to read EOF. Redirect all of them to /dev/null.
    let null = File::options().append(true).open("/dev/null")?;
    rustix::stdio::dup2_stdin(&null)?;
    rustix::stdio::dup2_stdout(&null)?;
    rustix::stdio::dup2_stderr(null)?;

    let mut hotplug = HotPlug::new(Arc::clone(&container), devices.clone(), symlinks)?;
    let hotplug_stream = hotplug.run();

    let container_stream = {
        let container = container.clone();
        async_stream::try_stream! {
            container.wait().await?;
            yield Event::Stopped;
        }
    };

    let mut stream = pin!(
        tokio_stream::empty()
            .merge(hotplug_stream)
            .merge(container_stream)
    );

    loop {
        let event = stream.try_next().await?.context("No more events")?;
        info!("{}", event);
        match event {
            Event::Initialized => {
                // Notify the parent process that we are ready to proceed.
                let notifier = notifier.take().context("Initialized event seen twice")?;
                rustix::io::write(notifier, &[0])?;
            }
            Event::Detach(dev) if devices.iter().any(|hub| dev.syspath() == hub) => {
                info!("Hub device detached. Stopping container.");
                let _ = container.kill(Signal::KILL).await;
                container.wait().await?;
                break;
            }
            Event::Stopped => {
                break;
            }
            _ => {}
        }
    }

    drop(ManuallyDrop::into_inner(container_keep));

    Ok(())
}

fn initialize_logger() {
    let log_env = env_logger::Env::default()
        .filter_or("LOG", "off")
        .write_style_or("LOG_STYLE", "auto");
    let logger = env_logger::Builder::new()
        .filter_module("container_hotplug", log::LevelFilter::Info)
        .format_target(false)
        .parse_env(log_env)
        .build();
    log::set_max_level(logger.filter());
    util::log::global_replace(Box::new(logger));
}

fn do_main() -> Result<()> {
    // This program is a wrapper around runc.
    //
    // When a container is started, runc is executed multiple times with different verbs:
    // * "create": create cgroup for the container
    // * "start": start the entrypoint
    // * "kill": kill the container (skipped if the container init exits cleanly)
    // * "delete": delete the cgroup
    //
    // We want to ensure that hotplug controller runs after the cgroup is created and last
    // until the cgroup is deleted. So we start a daemon process when we received the
    // "create" verb.
    //
    // To avoid having to communicate with the daemon process, the process
    // itself monitors the state of the container and exits when the cgroup is removed.
    // Therefore, we only need to intercept "create" command, and the rest can be forwarded
    // to runc directly.

    let args = runc::cli::Command::parse();

    initialize_logger();

    // Check that we're not running rootless. We need to control device access and must be root.
    if !rustix::process::geteuid().is_root() {
        bail!("This program must be run as root");
    }

    // Switch log target if specified.
    if let Some(log) = &args.global.log {
        let log_target = File::options()
            .create(true)
            .append(true)
            .open(log)
            .context("Cannot open log file")?;
        if args.global.log_format == runc::cli::LogFormat::Json {
            util::log::global_replace(Box::new(runc::log::JsonLogger::new(Box::new(log_target))));
        } else {
            rustix::stdio::dup2_stderr(log_target)?;
        }
    }

    let create_options = match args.command {
        runc::cli::Subcommand::Create(create_options) => create_options,
        runc::cli::Subcommand::Run { .. } => {
            // `run` is a shorthand for `create` and `start`.
            // It is never used by containerd shims, so there is no need to support it.
            bail!("container-hotplug does not support `run` command.");
        }
        runc::cli::Subcommand::Other(_) => Err(std::process::Command::new("runc")
            .args(std::env::args().skip(1))
            .exec())?,
    };

    // We need a daemon process to handle hotplug events.
    // To avoid having to serialize and deserialize all information, `fork`-ing is easiest.
    // However there is a limitation that `fork` is only safe in a single-threaded program.
    // `tokio` runtime is multithreaded, so we need to `fork` before that.
    // However for early forking, we need to know whether a container is successfully created.
    // We do this by creating a pipe. The pipe can be closed by either the child process exiting,
    // or the child process closing it.
    let (mut parent, child) = std::io::pipe()?;
    match safe_fork::fork().expect("should still be single-threaded") {
        None => {
            drop(parent);
            tokio::runtime::Runtime::new()?.block_on(create(args.global, create_options, child))?;
        }
        Some(pid) => {
            drop(child);
            if parent.read(&mut [0])? == 0 {
                // In this case, the child process exited before notifying us.
                std::process::exit(pid.join()?.code().unwrap_or(1) as _);
            }
        }
    }

    Ok(())
}

fn main() -> ExitCode {
    if let Err(err) = do_main() {
        log::error!("{:?}", err);
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
