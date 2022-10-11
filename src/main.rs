mod cli;
mod docker;
mod hotplug;
mod tokio_ext;
mod udev_ext;

use cli::{Device, Symlink};
use docker::{Container, Docker};
use hotplug::{Event as HotPlugEvent, HotPlug, PluggedDevice};

use std::fmt::Display;
use tokio_stream::StreamExt;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
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
        container.attach().await?.pipe_std()?;

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
async fn main() -> Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default()
            .filter_or("LOG", "off,container_ci_hotplug=debug")
            .write_style_or("LOG_STYLE", "auto"),
    )
    .init();

    let args = Args::parse();

    match args.action {
        Action::Run {
            root_device,
            symlink,
            docker_args,
        } => {
            let docker = Docker::connect_with_defaults()?;
            let container = docker.run(docker_args).await?;
            let _ = container.pipe_signals();
            let _guard = container.guard();

            let dev_path = root_device.device()?.syspath().to_owned();
            let hotplug_stream = run_ci_container(root_device, symlink, container.clone());
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
                    Event::Remove(dev) if dev.syspath() == dev_path => {
                        info!("Root device detached. Stopping container.");
                        container.remove(true).await?;
                    }
                    Event::Stopped(_, _) => {
                        break;
                    }
                    _ => {}
                }
            }
        }
    };

    Ok(())
}
