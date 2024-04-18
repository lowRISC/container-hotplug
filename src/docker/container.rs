use std::path::Path;
use std::pin::pin;
use std::sync::Arc;

use anyhow::{anyhow, ensure, Context, Error, Result};
use rustix::fs::{FileType, Gid, Mode, Uid};
use rustix::process::{Pid, Signal};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

use super::IoStream;
use crate::cgroup::{
    Access, DeviceAccessController, DeviceAccessControllerV1, DeviceAccessControllerV2, DeviceType,
};

pub struct Container {
    docker: bollard::Docker,
    id: String,
    pid: Pid,
    uid: Uid,
    gid: Gid,
    cgroup_device_filter: Mutex<Option<Box<dyn DeviceAccessController + Send>>>,
}

impl Container {
    pub(super) async fn new(docker: &bollard::Docker, id: String, pid: u32) -> Result<Self> {
        // Dropping the device filter will cause the container to have arbitrary device access.
        // So keep it alive until we're sure that the container is stopped.
        let cgroup_device_filter: Option<Box<dyn DeviceAccessController + Send>> =
            match DeviceAccessControllerV2::new(
                format!("/sys/fs/cgroup/system.slice/docker-{id}.scope").as_ref(),
            ) {
                Ok(v) => Some(Box::new(v)),
                Err(err2) => match DeviceAccessControllerV1::new(
                    format!("/sys/fs/cgroup/devices/docker/{id}").as_ref(),
                ) {
                    Ok(v) => Some(Box::new(v)),
                    Err(err1) => {
                        log::error!("neither cgroup v1 and cgroup v2 works");
                        log::error!("cgroup v2: {err2}");
                        log::error!("cgroup v1: {err1}");
                        None
                    }
                },
            };

        let mut this = Self {
            docker: docker.clone(),
            id,
            pid: Pid::from_raw(pid.try_into()?).context("Invalid PID")?,
            uid: Uid::ROOT,
            gid: Gid::ROOT,
            cgroup_device_filter: Mutex::new(cgroup_device_filter),
        };

        let uid = this.exec(&["id", "-u"]).await?.trim().parse()?;
        let gid = this.exec(&["id", "-g"]).await?.trim().parse()?;
        // Only invalid uid/gid for Linux is negative one.
        ensure!(uid != u32::MAX && gid != u32::MAX);
        // SAFETY: We just checked that the uid/gid is not -1.
        this.uid = unsafe { Uid::from_raw(uid) };
        this.gid = unsafe { Gid::from_raw(gid) };

        Ok(this)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub async fn exec<T: ToString>(&self, cmd: &[T]) -> Result<String> {
        let cmd = cmd.iter().map(|s| s.to_string()).collect();
        let options = bollard::exec::CreateExecOptions {
            cmd: Some(cmd),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            tty: Some(true),
            detach_keys: Some("ctrl-c".to_string()),
            ..Default::default()
        };
        let response = self.docker.create_exec::<String>(&self.id, options).await?;
        let id = response.id;

        let options = bollard::exec::StartExecOptions {
            detach: false,
            ..Default::default()
        };
        let response = self.docker.start_exec(&id, Some(options)).await?;
        let bollard::exec::StartExecResults::Attached {
            input: _,
            mut output,
        } = response
        else {
            unreachable!("we asked for attached IO streams");
        };

        let mut result = String::new();
        while let Some(output) = output.next().await {
            result.push_str(&output?.to_string());
        }
        Ok(result)
    }

    pub async fn attach(&self) -> Result<IoStream> {
        let options = bollard::container::AttachContainerOptions::<String> {
            stdin: Some(true),
            stdout: Some(true),
            stderr: Some(true),
            stream: Some(true),
            logs: Some(true),
            ..Default::default()
        };

        let response = self
            .docker
            .attach_container(&self.id, Some(options))
            .await?;

        Ok(IoStream {
            output: response.output,
            input: response.input,
            id: self.id.clone(),
            docker: self.docker.clone(),
        })
    }

    pub async fn name(&self) -> Result<String> {
        let inspect = self
            .docker
            .inspect_container(self.id.as_ref(), None)
            .await?;
        let name = inspect.name.context("Failed to obtain container name")?;
        Ok(name)
    }

    pub async fn kill(&self, signal: Signal) -> Result<()> {
        rustix::process::kill_process(self.pid, signal).context("Failed to kill container init")?;
        Ok(())
    }

    pub async fn wait(&self) -> Result<u8> {
        let options = bollard::container::WaitContainerOptions {
            condition: "not-running",
        };

        let response = self
            .docker
            .wait_container(self.id.as_str(), Some(options))
            .next()
            .await
            .context("No response received for wait")?;

        let code = match response {
            Ok(response) => response.status_code,
            // If the container does not complete, e.g. it's killed, then we will receive
            // an error code through docker.
            Err(bollard::errors::Error::DockerContainerWaitError { error: _, code }) => code,
            Err(err) => Err(err)?,
        };

        Ok(u8::try_from(code).unwrap_or(1))
    }

    pub async fn mknod(&self, node: &Path, (major, minor): (u32, u32)) -> Result<()> {
        crate::util::namespace::MntNamespace::of_pid(self.pid)?.enter(|| {
            if let Some(parent) = node.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::remove_file(node);
            rustix::fs::mknodat(
                rustix::fs::CWD,
                node,
                FileType::CharacterDevice,
                Mode::from(0o644),
                rustix::fs::makedev(major, minor),
            )?;
            if !self.uid.is_root() {
                rustix::fs::chown(node, Some(self.uid), Some(self.gid))?;
            }
            Ok(())
        })?
    }

    pub async fn symlink(&self, source: &Path, link: &Path) -> Result<()> {
        crate::util::namespace::MntNamespace::of_pid(self.pid)?.enter(|| {
            if let Some(parent) = link.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::remove_file(link);
            std::os::unix::fs::symlink(source, link)?;
            // No need to chown symlink. Permission is determined by the target.
            Ok(())
        })?
    }

    pub async fn rm(&self, node: &Path) -> Result<()> {
        crate::util::namespace::MntNamespace::of_pid(self.pid)?.enter(|| {
            let _ = std::fs::remove_file(node);
        })
    }

    pub async fn device(&self, (major, minor): (u32, u32), access: Access) -> Result<()> {
        let mut controller = self.cgroup_device_filter.lock().await;
        controller
            .as_mut()
            .context("Device controller does not exist")?
            .set_permission(DeviceType::Character, major, minor, access)?;
        Ok(())
    }

    pub async fn pipe_signals(self: Arc<Self>) -> JoinHandle<Result<()>> {
        let container = Arc::downgrade(&self);
        tokio::spawn(async move {
            let mut stream = pin!(signal_stream(SignalKind::alarm())
                .merge(signal_stream(SignalKind::hangup()))
                .merge(signal_stream(SignalKind::interrupt()))
                .merge(signal_stream(SignalKind::quit()))
                .merge(signal_stream(SignalKind::terminate()))
                .merge(signal_stream(SignalKind::user_defined1()))
                .merge(signal_stream(SignalKind::user_defined2())));

            while let Some(signal) = stream.next().await {
                container
                    .upgrade()
                    .context("Container dropped")?
                    .kill(Signal::from_raw(signal?.as_raw_value()).unwrap())
                    .await?;
            }

            Err::<_, Error>(anyhow!("Failed to listen for signals"))
        })
    }
}

fn signal_stream(kind: SignalKind) -> impl tokio_stream::Stream<Item = Result<SignalKind>> {
    async_stream::try_stream! {
        let sig_kind = SignalKind::hangup();
        let mut sig_stream = signal(kind)?;
        while sig_stream.recv().await.is_some() {
            yield sig_kind;
        }
    }
}
