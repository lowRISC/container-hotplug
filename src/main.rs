mod cgroup;
mod cli;
mod dev;
mod docker;
mod hotplug;
mod util;

use cli::{Action, DeviceRef, Symlink};
use docker::{Container, Docker};
use hotplug::{Event as HotPlugEvent, HotPlug, PluggedDevice};

use std::fmt::Display;
use std::pin::pin;
use tokio_stream::StreamExt;

use anyhow::Result;
use clap::Parser;
use clap_verbosity_flag::{InfoLevel, LogLevel, Verbosity};
use log::info;

#[derive(Clone)]
enum Event {
    Add(PluggedDevice),
    Remove(PluggedDevice),
    Initialized,
    Stopped(i64),
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
            Event::Initialized => {
                write!(f, "Container initialized")
            }
            Event::Stopped(status) => {
                write!(f, "Container exited with status {status}")
            }
        }
    }
}

fn run_hotplug(
    device: DeviceRef,
    symlinks: Vec<Symlink>,
    container: Container,
    verbosity: Verbosity<impl LogLevel>,
) -> impl tokio_stream::Stream<Item = Result<Event>> {
    async_stream::try_stream! {
        let name = container.name().await?;
        let id = container.id();
        info!("Attaching to container {name} ({id})");

        let hub_path = device.device()?.syspath().to_owned();
        let mut hotplug = HotPlug::new(container.clone(), hub_path.clone(), symlinks)?;

        {
            let events = hotplug.start();
            tokio::pin!(events);
            while let Some(event) = events.next().await {
                yield Event::from(event?);
            }
        }

        yield Event::Initialized;

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

async fn run(param: cli::Run, verbosity: Verbosity<InfoLevel>) -> Result<u8> {
    let mut status = 0;

    let docker = Docker::connect_with_defaults()?;
    let container = docker.run(param.docker_args).await?;
    drop(container.pipe_signals());

    let hub_path = param.root_device.device()?.syspath().to_owned();
    let hotplug_stream = run_hotplug(
        param.root_device,
        param.symlink,
        container.clone(),
        verbosity,
    );
    let container_stream = {
        let container = container.clone();
        async_stream::try_stream! {
            let status = container.wait().await?;
            yield Event::Stopped(status)
        }
    };

    let stream = pin!(tokio_stream::empty()
        .merge(hotplug_stream)
        .merge(container_stream));

    let result: Result<()> = async {
        tokio::pin!(stream);
        while let Some(event) = stream.next().await {
            let event = event?;
            info!("{}", event);
            match event {
                Event::Remove(dev) if dev.syspath() == hub_path => {
                    info!("Hub device detached. Stopping container.");
                    status = param.root_unplugged_exit_code;
                    container.kill(15).await?;
                    break;
                }
                Event::Stopped(code) => {
                    status = 1;
                    if let Ok(code) = u8::try_from(code) {
                        // Use the container exit code, but only if it won't be confused
                        // with the pre-defined root_unplugged_exit_code.
                        if code != param.root_unplugged_exit_code {
                            status = code;
                        }
                    } else {
                        status = 1;
                    }
                    break;
                }
                _ => {}
            }
        }
        Ok(())
    }
    .await;

    let _ = container.remove(param.remove_timeout).await;
    result?;
    Ok(status)
}

#[tokio::main]
async fn main() {
    let args = cli::Args::parse();

    let log_env = env_logger::Env::default()
        .filter_or("LOG", "off")
        .write_style_or("LOG_STYLE", "auto");

    env_logger::Builder::from_env(log_env)
        .filter_module("container_hotplug", args.verbosity.log_level_filter())
        .format_timestamp(if args.log_format.timestamp {
            Some(Default::default())
        } else {
            None
        })
        .format_module_path(args.log_format.path)
        .format_target(args.log_format.module)
        .format_level(args.log_format.level)
        .init();

    let result = match args.action {
        Action::Run(param) => run(param, args.verbosity).await,
    };
    let code = match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("Error: {err:?}");
            1
        }
    };
    // Upon returning from `main`, tokio will attempt to shutdown, but if there're any blocking
    // operation (e.g. fs operations), then the shutdown will hang.
    std::process::exit(code.into());
}
