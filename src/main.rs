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
use std::pin::pin;
use std::sync::Arc;
use tokio_stream::StreamExt;

use anyhow::Result;
use clap::Parser;
use clap_verbosity_flag::{InfoLevel, Verbosity};
use log::info;

#[derive(Clone)]
enum Event {
    Attach(AttachedDevice),
    Detach(AttachedDevice),
    Initialized,
    Stopped(i64),
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

async fn run(param: cli::Run, verbosity: Verbosity<InfoLevel>) -> Result<u8> {
    let hub_path = param.root_device.device()?.syspath().to_owned();

    let mut status = 0;

    let docker = Docker::connect_with_defaults()?;
    let container = Arc::new(docker.run(param.docker_args).await?);
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

    let stream = pin!(tokio_stream::empty()
        .merge(hotplug_stream)
        .merge(container_stream));

    let result: Result<()> = async {
        tokio::pin!(stream);
        while let Some(event) = stream.next().await {
            let event = event?;
            info!("{}", event);
            match event {
                Event::Initialized => {
                    if !verbosity.is_silent() {
                        container.attach().await?.pipe_std();
                    }
                }
                Event::Detach(dev) if dev.syspath() == hub_path => {
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
            log::error!("{err:?}");
            125
        }
    };
    // Upon returning from `main`, tokio will attempt to shutdown, but if there're any blocking
    // operation (e.g. fs operations), then the shutdown will hang.
    std::process::exit(code.into());
}
