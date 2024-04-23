mod cgroup;
mod cli;
mod dev;
mod docker;
mod hotplug;
mod util;

use cli::Action;
use docker::Docker;
use hotplug::{AttachedDevice, HotPlug};

use std::fmt::Display;
use std::mem::ManuallyDrop;
use std::pin::pin;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;
use log::info;
use rustix::process::Signal;
use tokio_stream::StreamExt;

#[derive(Clone)]
enum Event {
    Attach(AttachedDevice),
    Detach(AttachedDevice),
    Initialized,
    Stopped(u8),
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
            Event::Stopped(status) => {
                write!(f, "Container exited with status {status}")
            }
        }
    }
}

async fn run(param: cli::Run) -> Result<u8> {
    let hub_path = param.root_device.device()?.syspath().to_owned();

    let docker = Docker::connect_with_defaults()?;
    let container = Arc::new(docker.run(param.docker_args).await?);
    // Dropping the `Container` will detach the device cgroup filter.
    // To prevent accidentally detaching it, wrap it in `ManuallyDrop` and only do so
    // when we're certain that the container stopped.
    let container_keep = ManuallyDrop::new(container.clone());
    drop(container.clone().pipe_signals());

    info!(
        "Attaching to container {} ({})",
        container.name().await?,
        container.id()
    );

    let mut hotplug = HotPlug::new(container.clone(), hub_path.clone(), param.symlink)?;
    let hotplug_stream = hotplug.run();

    let container_stream = {
        let container = container.clone();
        async_stream::try_stream! {
            let status = container.wait().await?;
            yield Event::Stopped(status)
        }
    };

    let mut stream = pin!(tokio_stream::empty()
        .merge(hotplug_stream)
        .merge(container_stream));

    let status = loop {
        let event = stream.try_next().await?.context("No more events")?;
        info!("{}", event);
        match event {
            Event::Initialized => {
                container.attach().await?.pipe_std();
            }
            Event::Detach(dev) if dev.syspath() == hub_path => {
                info!("Hub device detached. Stopping container.");
                container.kill(Signal::Kill).await?;

                let _ = container.wait().await?;
                break param.root_unplugged_exit_code;
            }
            Event::Stopped(code) => {
                // Use the container exit code, but only if it won't be confused
                // with the pre-defined root_unplugged_exit_code.
                if code != param.root_unplugged_exit_code {
                    break code;
                } else {
                    break 1;
                }
            }
            _ => {}
        }
    };

    drop(ManuallyDrop::into_inner(container_keep));

    Ok(status)
}

fn initialize_logger() {
    let log_env = env_logger::Env::default()
        .filter_or("LOG", "off")
        .write_style_or("LOG_STYLE", "auto");
    env_logger::Builder::new()
        .filter_module("container_hotplug", log::LevelFilter::Info)
        .format_target(false)
        .parse_env(log_env)
        .init();
}

fn do_main() -> Result<u8> {
    let args = cli::Args::parse();

    initialize_logger();

    // Check that we're not running rootless. We need to control device access and must be root.
    if !rustix::process::geteuid().is_root() {
        bail!("This program must be run as root");
    }

    let param = match args.action {
        Action::Run(param) => param,
    };

    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(run(param));
    rt.shutdown_background();
    result
}

fn main() -> ExitCode {
    match do_main() {
        Ok(code) => code.into(),
        Err(err) => {
            log::error!("{:?}", err);
            ExitCode::from(125)
        }
    }
}
