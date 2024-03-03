use crate::cli::Timeout;

use super::{IoStream, IoStreamSource};

use anyhow::{anyhow, Context, Error, Result};
use bollard::service::EventMessage;
use futures::future::{BoxFuture, Shared};
use tokio::io::AsyncWriteExt;
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::{spawn, JoinHandle};
use tokio_stream::StreamExt;

#[derive(Clone)]
pub struct Container {
    pub(super) id: String,
    pub(super) docker: bollard::Docker,
    pub(super) remove_event: Shared<BoxFuture<'static, Option<EventMessage>>>,
}

pub struct ContainerGuard(Option<Container>, Timeout);

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let container = self.0.take().unwrap();
        let timeout = self.1;
        let _ = futures::executor::block_on(container.remove(timeout));
    }
}

impl Container {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn guard(&self, timeout: Timeout) -> ContainerGuard {
        ContainerGuard(Some(self.clone()), timeout)
    }

    pub async fn remove(&self, timeout: Timeout) -> Result<()> {
        self.rename(format!("removing-{}", self.id)).await?;
        let options = bollard::container::RemoveContainerOptions {
            force: true,
            ..Default::default()
        };
        let _ = self.docker.remove_container(&self.id, Some(options)).await;
        if let Timeout::Some(duration) = timeout {
            let _ = tokio::time::timeout(duration, self.remove_event.clone()).await;
        } else {
            self.remove_event.clone().await;
        }
        Ok(())
    }

    pub async fn rename<U: AsRef<str>>(&self, name: U) -> Result<()> {
        let required = bollard::container::RenameContainerOptions {
            name: name.as_ref(),
        };
        self.docker.rename_container(&self.id, required).await?;
        Ok(())
    }

    pub async fn exec<U: AsRef<str>, T: AsRef<[U]>>(&self, cmd: T) -> Result<IoStream> {
        let iter = cmd.as_ref().iter().map(|s| s.as_ref().into());
        let options = bollard::exec::CreateExecOptions {
            cmd: Some(iter.collect()),
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

        if let bollard::exec::StartExecResults::Attached { input, output } = response {
            return Ok(IoStream {
                output,
                input,
                source: IoStreamSource::Exec(id),
                docker: self.docker.clone(),
            });
        }

        unreachable!();
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
            source: IoStreamSource::Container(self.id.clone()),
            docker: self.docker.clone(),
        })
    }

    async fn inspect(&self) -> Result<bollard::models::ContainerInspectResponse> {
        Ok(self
            .docker
            .inspect_container(self.id.as_ref(), None)
            .await?)
    }

    pub async fn name(&self) -> Result<String> {
        let inspect = self.inspect().await?;
        let name = inspect.name.context("Failed to obtain container name")?;
        Ok(name)
    }

    pub async fn kill(&self, signal: i32) -> Result<()> {
        let options = bollard::container::KillContainerOptions {
            signal: format!("{}", signal),
        };
        self.docker.kill_container(&self.id, Some(options)).await?;
        Ok(())
    }

    pub async fn wait(&self) -> Result<i64> {
        let options = bollard::container::WaitContainerOptions {
            condition: "not-running",
        };

        let response = self
            .docker
            .wait_container(self.id.as_str(), Some(options))
            .next()
            .await
            .context("No response received for wait")?;

        match response {
            Ok(response) => Ok(response.status_code),
            // If the container does not complete, e.g. it's killed, then we will receive
            // an error code through docker.
            Err(bollard::errors::Error::DockerContainerWaitError { error: _, code }) => Ok(code),
            Err(err) => Err(err)?,
        }
    }

    pub async fn mkdir<T: AsRef<std::path::Path>>(&self, path: T) -> Result<()> {
        self.exec(["mkdir", "-p", &path.as_ref().to_string_lossy()])
            .await?
            .collect()
            .await?;
        Ok(())
    }

    pub async fn mkdir_for<T: AsRef<std::path::Path>>(&self, path: T) -> Result<()> {
        if let Some(path) = path.as_ref().parent() {
            self.mkdir(path).await?;
        }
        Ok(())
    }

    pub async fn mknod<T: AsRef<std::path::Path>>(
        &self,
        node: T,
        (major, minor): (u32, u32),
    ) -> Result<()> {
        self.rm(&node).await?;
        self.mkdir_for(&node).await?;
        self.exec([
            "mknod",
            &node.as_ref().to_string_lossy(),
            "c",
            &major.to_string(),
            &minor.to_string(),
        ])
        .await?
        .collect()
        .await?;
        Ok(())
    }

    pub async fn symlink<T: AsRef<std::path::Path>, U: AsRef<std::path::Path>>(
        &self,
        source: T,
        link: U,
    ) -> Result<()> {
        self.mkdir_for(&link).await?;
        self.exec([
            "ln",
            "-sf",
            &source.as_ref().to_string_lossy(),
            &link.as_ref().to_string_lossy(),
        ])
        .await?
        .collect()
        .await?;
        Ok(())
    }

    pub async fn rm<T: AsRef<std::path::Path>>(&self, node: T) -> Result<()> {
        self.exec(["rm", "-f", &node.as_ref().to_string_lossy()])
            .await?
            .collect()
            .await?;
        Ok(())
    }

    pub async fn device(
        &self,
        (major, minor): (u32, u32),
        (r, w, m): (bool, bool, bool),
    ) -> Result<()> {
        let mut permissions = String::new();
        if r {
            permissions.push('r');
        }
        if w {
            permissions.push('w');
        }
        if m {
            permissions.push('m');
        }

        deny_device_cgroup1(&self.id, major, minor, "rwm").await?;

        if permissions != "" {
            allow_device_cgroup1(&self.id, major, minor, permissions.as_ref()).await?;
        }

        Ok(())
    }

    pub async fn pipe_signals(&self) -> JoinHandle<Result<()>> {
        let container = self.clone();
        let handle = spawn(async move {
            let stream = tokio_stream::empty()
                .merge(signal_stream(SignalKind::alarm()))
                .merge(signal_stream(SignalKind::hangup()))
                .merge(signal_stream(SignalKind::interrupt()))
                .merge(signal_stream(SignalKind::quit()))
                .merge(signal_stream(SignalKind::terminate()))
                .merge(signal_stream(SignalKind::user_defined1()))
                .merge(signal_stream(SignalKind::user_defined2()));

            tokio::pin!(stream);
            while let Some(signal) = stream.next().await {
                container.kill(signal?.as_raw_value()).await?;
            }

            Err::<(), Error>(anyhow!("Failed to listen for signals"))
        });

        let container = self.clone();
        spawn(async move {
            let _ = container.wait().await;
            handle.abort();
            Ok::<(), Error>(())
        })
    }
}

async fn allow_device_cgroup1(
    container_id: &str,
    major: u32,
    minor: u32,
    permissions: &str,
) -> Result<()> {
    let path = format!("/sys/fs/cgroup/devices/docker/{container_id}/devices.allow");
    let mut file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
    let mut data = bytes::Bytes::from(format!("c {major}:{minor} {permissions}"));
    file.write_all_buf(&mut data).await?;
    Ok(())
}

async fn deny_device_cgroup1(
    container_id: &str,
    major: u32,
    minor: u32,
    permissions: &str,
) -> Result<()> {
    let path = format!("/sys/fs/cgroup/devices/docker/{container_id}/devices.deny");
    let mut file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
    let mut data = bytes::Bytes::from(format!("c {major}:{minor} {permissions}"));
    file.write_all_buf(&mut data).await?;
    Ok(())
}

fn signal_stream(kind: SignalKind) -> impl tokio_stream::Stream<Item = Result<SignalKind>> {
    async_stream::try_stream! {
        let sig_kind = SignalKind::hangup();
        let mut sig_stream = signal(kind)?;
        while let Some(_) = sig_stream.recv().await {
            yield sig_kind;
        }
    }
}
