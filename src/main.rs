mod cli;
mod docker;
mod hotplug;

use cli::{Device, Symlink, LogFormat};
use docker::{Container, Docker};
use hotplug::{Event as HotPlugEvent, HotPlug, PluggedDevice};

use std::{fmt::Display, process::ExitCode};
use tokio_stream::StreamExt;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use clap_verbosity_flag::{InfoLevel, LogLevel, Verbosity};
use log::info;

use crate::hotplug::PluggableDevice;

#[derive(Parser)]
struct Args {
    #[clap(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    Run {
        #[clap(flatten)]
        verbosity: Verbosity<InfoLevel>,
        #[clap(short = 'L', long, default_value = "")]
        log_format: LogFormat,
        #[arg(short = 'd', long)]
        root_device: Device,
        #[arg(short = 'l', long)]
        symlink: Vec<Symlink>,
        #[arg(trailing_var_arg = true)]
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

fn run_ci_container(
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

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let args = Args::parse();
    let mut status = ExitCode::SUCCESS;

    match args.action {
        Action::Run {
            verbosity,
            log_format,
            root_device,
            symlink,
            docker_args,
        } => {
            let log_env = env_logger::Env::default()
                .filter_or("LOG", "off")
                .write_style_or("LOG_STYLE", "auto");
            
            env_logger::Builder::from_env(log_env)
                .filter_module("container_ci_hotplug", verbosity.log_level_filter())
                .format_timestamp(if log_format.timestamp { Some(Default::default()) } else { None })
                .format_module_path(log_format.path)
                .format_target(log_format.module)
                .format_level(log_format.level)
                .init();

            let docker = Docker::connect_with_defaults()?;
            let container = docker.run(docker_args).await?;
            let _ = container.pipe_signals();
            let _guard = container.guard();

            let hub_path = root_device.hub()?.syspath().to_owned();
            let hotplug_stream =
                run_ci_container(root_device, symlink, container.clone(), verbosity);
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
                        status = ExitCode::from(5);
                        container.kill(15).await.ok();
                        container.remove(true).await.ok();
                        break;
                    }
                    Event::Stopped(_, _) => {
                        break;
                    }
                    _ => {}
                }
            }
        }
    };

    Ok(status)
}
