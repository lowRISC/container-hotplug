mod cli;
mod docker;
mod hotplug;

use cli::{Device, LogFormat, Symlink, Timeout};
use docker::{Container, Docker};
use hotplug::{Event as HotPlugEvent, HotPlug, PluggedDevice};

use std::{fmt::Display, process::ExitCode, path::Path};
use tokio_stream::StreamExt;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use clap_verbosity_flag::{InfoLevel, LogLevel, Verbosity};
use log::info;

use crate::hotplug::PluggableDevice;

#[derive(Parser)]
#[command(max_term_width = 180)]
struct Args {
    #[clap(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Wraps a call to `docker run` to allow hot-plugging devices into a
    /// container as they are plugged
    Run {
        #[arg(short = 'd', long, id = "DEVICE")]
        /// Root hotplug device: [[parent-of:]*]<PREFIX>:<DEVICE> {n}
        /// PREFIX can be: {n}
        ///  - usb: A USB device identified as <VID>[:<PID>[:<SERIAL>]] {n}
        ///  - syspath: A directory path in /sys/** {n}
        ///  - devnode: A device path in /dev/** {n}
        /// e.g., parent-of:usb:2b3e:c310
        root_device: Device,

        #[arg(short = 'l', long, id = "SYMLINK")]
        /// Create a symlink for a device: <PREFIX>:<DEVICE>=<PATH> {n}
        /// PREFIX can be: {n}
        ///  - usb: A USB device identified as <VID>:<PID>:<INTERFACE> {n}
        /// e.g., usb:2b3e:c310:1=/dev/ttyACM_CW310_0
        symlink: Vec<Symlink>,

        #[clap(short = 'u', long, default_value = "5", id = "CODE")]
        /// Exit code to return when the root device is unplugged
        root_unplugged_exit_code: u8,

        #[clap(short = 't', long, default_value = "20s", id = "TIMEOUT")]
        /// Timeout when waiting for the container to be removed
        remove_timeout: Timeout,

        #[clap(flatten)]
        verbosity: Verbosity<InfoLevel>,

        #[clap(short = 'L', long, default_value = "+l-pmt", id = "FORMAT")]
        /// Log mesage format: [[+-][ltmp]*]* {n}
        ///   +/-: enable/disable {n}
        ///   l: level {n}
        ///   t: timestamp {n}
        ///   m/p: module name/path {n}
        /// 
        log_format: LogFormat,

        #[arg(trailing_var_arg = true, id = "ARGS")]
        /// Arguments to pass to `docker run`
        docker_args: Vec<String>,
    },
}

#[derive(Clone)]
enum Event {
    Add(PluggedDevice),
    Remove(PluggedDevice),
    Initialized(Container),
    Stopped(Container, i64),
}

impl From<HotPlugEvent> for Event {
    fn from(evt: HotPlugEvent) -> Self {
        match evt {
            HotPlugEvent::Add(dev) => Self::Add(dev),
            HotPlugEvent::Remove(dev) => Self::Remove(dev),
        }
    }
}

impl Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::Add(dev) => {
                write!(f, "Attaching device {dev}")
            }
            Event::Remove(dev) => {
                write!(f, "Detaching device {dev}")
            }
            Event::Initialized(_) => {
                write!(f, "Container initialized")
            }
            Event::Stopped(_, status) => {
                write!(f, "Container exited with status {status}")
            }
        }
    }
}

fn run_hotplug(
    device: Device,
    symlinks: Vec<Symlink>,
    container: Container,
    verbosity: Verbosity<impl LogLevel>,
) -> impl tokio_stream::Stream<Item = Result<Event>> {
    async_stream::try_stream! {
        let name = container.name().await?;
        let id = container.id();
        info!("Attaching to container {name} ({id})");

        let hub_path = device.hub()?.syspath().to_owned();
        let device = PluggableDevice::from_device(&device.device()?)
            .context("Failed to obtain basic device information")?;

        let mut hotplug = HotPlug::new(container.clone(), device, hub_path.clone(), symlinks)?;

        {
            let events = hotplug.start();
            tokio::pin!(events);
            while let Some(event) = events.next().await {
                yield Event::from(event?);
            }
        }

        yield Event::Initialized(container.clone());

        if !verbosity.is_silent() {
            container.attach().await?.pipe_std();
        }

        {
            let events = hotplug.run();
            tokio::pin!(events);
            while let Some(event) = events.next().await {
                yield Event::from(event?);
            }
        }
    }
}

async fn hotplug_main() -> Result<ExitCode> {
    let args = Args::parse();
    let mut status = ExitCode::SUCCESS;

    match args.action {
        Action::Run {
            verbosity,
            log_format,
            remove_timeout,
            root_unplugged_exit_code,
            root_device,
            symlink,
            docker_args,
        } => {
            let log_env = env_logger::Env::default()
                .filter_or("LOG", "off")
                .write_style_or("LOG_STYLE", "auto");

            if !Path::new("/sys/fs/cgroup/devices/").is_dir() {
                bail!("Could not find cgroup v1");
            }

            env_logger::Builder::from_env(log_env)
                .filter_module("container_hotplug", verbosity.log_level_filter())
                .format_timestamp(if log_format.timestamp {
                    Some(Default::default())
                } else {
                    None
                })
                .format_module_path(log_format.path)
                .format_target(log_format.module)
                .format_level(log_format.level)
                .init();

            let docker = Docker::connect_with_defaults()?;
            let container = docker.run(docker_args).await?;
            let _guard = container.guard(remove_timeout);
            let _ = container.pipe_signals();

            let hub_path = root_device.hub()?.syspath().to_owned();
            let hotplug_stream =
                run_hotplug(root_device, symlink, container.clone(), verbosity);
            let container_stream = {
                let container = container.clone();
                async_stream::try_stream! {
                    let status = container.wait().await?;
                    yield Event::Stopped(container.clone(), status)
                }
            };

            let stream = tokio_stream::empty()
                .merge(hotplug_stream)
                .merge(container_stream);

            tokio::pin!(stream);
            while let Some(event) = stream.next().await {
                let event = event?;
                info!("{}", event);
                match event {
                    Event::Remove(dev) if dev.syspath() == hub_path => {
                        info!("Hub device detached. Stopping container.");
                        status = ExitCode::from(root_unplugged_exit_code);
                        container.kill(15).await?;
                        break;
                    }
                    Event::Stopped(_, code) => {
                        if code >= 0 && code < 256 && (code as u8) != root_unplugged_exit_code {
                            status = ExitCode::from(code as u8);
                        } else {
                            status = ExitCode::FAILURE;
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    };

    Ok(status)
}

#[tokio::main]
async fn main() -> ExitCode {
    match hotplug_main().await {
        Ok(code) => code,
        Err(err) => {
            let _ = eprintln!("Error: {err:?}");
            ExitCode::FAILURE
        }
    }
}
