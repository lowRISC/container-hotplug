mod docker;
mod hotplug;
mod tokio_ext;
mod udev_device_ext;

use docker::{Container, Docker, RestartPolicy};
use hotplug::HotPlug;
use tokio_ext::WithJoinHandleGuard;

use std::{env::current_dir, ffi::OsStr, future::Future, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use tokio::time::sleep;

use tokio_udev::Device;
use tokio_udev::Enumerator;

#[derive(Parser)]
struct Args {
    #[clap(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    Run {
        #[arg(short, long)]
        serial: Option<String>,
    },
    HotPlug {
        container_id: String,
        #[arg(short, long)]
        serial: Option<String>,
    },
}

fn find_device(
    vid: impl AsRef<OsStr>,
    pid: impl AsRef<OsStr>,
    serial: Option<impl AsRef<OsStr>>,
) -> Result<Option<Device>> {
    let mut enumerator = Enumerator::new()?;
    enumerator.match_attribute("idVendor", vid)?;
    enumerator.match_attribute("idProduct", pid)?;
    if let Some(serial) = serial {
        enumerator.match_attribute("serial", serial)?;
    }

    Ok(enumerator.scan_devices()?.next())
}

async fn run_ci_container<'a, Fut, F>(device: &Device, get_container: F) -> Result<()>
where
    Fut: Future<Output = Result<Container>>,
    F: FnOnce() -> Fut,
{
    // let device_syspath = device.syspath();
    let hub = device.parent().context("Failed to get CW310 parent hub")?;
    let container = get_container().await?;
    container.ensure_running().await?;

    let symlinks = [
        // NewAE ChipWhisperer CW310
        (("2b3e", "c310", "01"), "/dev/ttyACM_CW310_0"),
        (("2b3e", "c310", "03"), "/dev/ttyACM_CW310_1"),
        // Olimex ARM-USB-TINY-H JTAG
        (("15ba", "002a", "01"), "/dev/ttyUSB_JTAG_0"),
        // OpenTitan USB device
        (("18d1", "503a", "00"), "/dev/ttyUSB_OT_0"),
        (("18d1", "503a", "01"), "/dev/ttyUSB_OT_1"),
    ];

    let _hotplug_guard = container.hotplug(&hub, symlinks).guard();
    sleep(Duration::from_secs(2)).await;

    let name = container.name().await?;
    let id = container.id();
    println!("\x1b[1;32mContainer {name} ({id})\x1b[0m");

    container
        .bash(
            "
            if ! [ -e /usr/lib/x86_64-linux-gnu/libftdi1.so.2 ]; then
                echo -e '\x1b[1;32mInstalling libftdi\x1b[0m'
                DEBIAN_FRONTEND=noninteractive
                apt-get update > /dev/null
                apt-get install -y --no-install-recommends libftdi1-2 > /dev/null 2>&1
            fi",
        )
        .await?
        .pipe_std()
        .await?;

    println!("\x1b[1;32mRunning opentitantool\x1b[0m");

    // #/ott/ott.sh load-bitstream /ott/bitstream.bit
    // #/ott/ott.sh set-pll
    // {
    //     sleep 2
    //     echo -ne 'hola mundo!\\r\\n'
    // } | /ott/ott.sh console --timeout=4sec
    container
        .bash("/ott/ott.sh bootstrap /ott/hello_usbdev_fpga_cw310.bin")
        .await?
        .pipe_std()
        .await?;

    println!("\x1b[1;32mAwaiting container\x1b[0m");

    let status = container.wait().await?;
    println!("\x1b[1;32mContainer exited with status code {status}\x1b[0m");
    println!("\x1b[1;32mBye!\x1b[0m");

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    match args.action {
        Action::Run { serial } => {
            let device = find_device("2b3e", "c310", serial)?.context("Failed to find device")?;

            let serial = device
                .attribute_value("serial")
                .context("Failed to get CW310 serial")?;
            let name = format!("ci-container-eda-{}", serial.to_string_lossy());

            let get_container = || async move {
                Docker::connect_with_defaults()?
                    .with_image("ubuntu:22.04")
                    .name(&name)
                    .remove_old(true)
                    .restart_policy(RestartPolicy::NO)
                    .auto_remove(true)
                    .bind([format!("{}/ott:/ott", current_dir()?.display())])
                    .bash(
                        "
                        while ! [ -e /stop ]; do
                            sleep 1s
                        done",
                    )
                    .create()
                    .await
            };

            run_ci_container(&device, get_container).await
        }
        Action::HotPlug {
            container_id,
            serial,
        } => {
            let device = find_device("2b3e", "c310", serial)?.context("Failed to find device")?;

            let get_container = || async move {
                Docker::connect_with_defaults()?
                    .get_container(container_id)
                    .await
            };

            run_ci_container(&device, get_container).await
        }
    }
}
