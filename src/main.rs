mod cli;
mod docker;
mod hotplug;
mod tokio_ext;
mod udev_ext;

use cli::{Device, Symlink};
use docker::{Container, Docker};
use hotplug::{HotPlug, HotPlugEvent};

use std::future::Future;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use log::{debug, info};

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
    Attach {
        container_id: String,
        #[arg(short = 'd', long)]
        root_device: Device,
        #[arg(short = 'l', long)]
        symlink: Vec<Symlink>,
    },
}

fn log_event(event: HotPlugEvent) {
    match event {
        HotPlugEvent::Add(dev) => {
            debug!("Attaching device {dev}");
        }
        HotPlugEvent::Remove(dev) => {
            debug!("Detaching device {dev}");
        }
        HotPlugEvent::ReAdd(new, old) => {
            debug!("Reattaching device {new} (was {old})");
        }
        HotPlugEvent::RequiredRemoved(dev) => {
            debug!("Detaching required device {dev}");
            debug!("Container will be stopped now");
        }
        HotPlugEvent::RequiredMissing(devs) => {
            for (_, (major, minor), devnode) in devs {
                debug!(
                    "Missing required device {major:0>3}:{minor:0>3} [{}]",
                    devnode.display()
                );
            }
        }
    }
}

async fn run_ci_container<'a, Fut, F>(
    device: &Device,
    symlinks: Vec<Symlink>,
    get_container: F,
) -> Result<()>
where
    Fut: Future<Output = Result<Container>>,
    F: FnOnce() -> Fut,
{
    let dev = device.device()?;
    let hub = device
        .hub()
        .context(anyhow!("Failed to get device `{}`", device.id()))?;
    let container = get_container().await?;
    container.ensure_running().await?;

    let name = container.name().await?;
    let id = container.id();
    info!("Attaching to container {name} ({id})");

    let on_start = |container: &Container| {
        let container = container.clone();
        tokio::spawn(async move {
            container.attach().await?.pipe_std()?;
            Ok::<(), anyhow::Error>(())
        });
    };

    let status = container
        .hotplug(dev, hub, symlinks, on_start, log_event)
        .await??;
    info!("Container {name} ({id}) exited with status code {status}");

    Ok(())
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
            let get_container = || async move {
                let docker = Docker::connect_with_defaults()?;
                let container = docker.run(docker_args).await?;
                Ok(container)
            };

            run_ci_container(&root_device, symlink, get_container).await?;
        }
        Action::Attach {
            container_id,
            root_device,
            symlink,
        } => {
            let get_container = || async move {
                Docker::connect_with_defaults()?
                    .get_container(container_id)
                    .await
            };

            run_ci_container(&root_device, symlink, get_container).await?;
        }
    };

    Ok(())
}
