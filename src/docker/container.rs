use super::{IoStream, IoStreamSource};

use anyhow::{anyhow, Context, Error, Result};
use tokio::io::AsyncWriteExt;
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::{spawn, JoinHandle};
use tokio_stream::StreamExt;

#[derive(Clone)]
pub struct Container(pub(super) String, pub(super) bollard::Docker);

pub struct ContainerGuard(Container);

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let container = self.0.clone();
        spawn(async move {
            container.remove(true).await.ok();
        });
    }
}

impl Container {
    pub fn id(&self) -> &str {
        &self.0
    }

    pub fn guard(&self) -> ContainerGuard {
        ContainerGuard(self.clone())
    }

    pub async fn remove(&self, force: bool) -> Result<()> {
        let options = bollard::container::RemoveContainerOptions {
            force,
            ..Default::default()
        };
        self.1.remove_container(&self.0, Some(options)).await?;
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
        let response = self.1.create_exec::<String>(&self.0, options).await?;
        let id = response.id;

        let options = bollard::exec::StartExecOptions {
            detach: false,
            ..Default::default()
        };
        let response = self.1.start_exec(&id, Some(options)).await?;

        if let bollard::exec::StartExecResults::Attached { input, output } = response {
            return Ok(IoStream {
                output,
                input,
                source: IoStreamSource::Exec(id),
                docker: self.1.clone(),
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

        let response = self.1.attach_container(&self.0, Some(options)).await?;

        Ok(IoStream {
            output: response.output,
            input: response.input,
            source: IoStreamSource::Container(self.0.clone()),
            docker: self.1.clone(),
        })
    }

    async fn inspect(&self) -> Result<bollard::models::ContainerInspectResponse> {
        Ok(self.1.inspect_container(self.0.as_ref(), None).await?)
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
        self.1.kill_container(&self.0, Some(options)).await?;
        Ok(())
    }

    pub async fn wait(&self) -> Result<i64> {
        let options = bollard::container::WaitContainerOptions {
            condition: "not-running",
        };
        let mut response = self.1.wait_container(self.0.as_str(), Some(options));

        let mut last = None;
        while let Some(wait_response) = response.next().await {
            last = Some(wait_response?);
        }

        anyhow::ensure!(last.is_some(), "Unexpected exit status");

        Ok(last.unwrap().status_code)
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
        (major, minor): (u64, u64),
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
        (major, minor): (u64, u64),
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

        deny_device_cgroup1(&self.0, major, minor, "rwm").await?;

        if permissions != "" {
            allow_device_cgroup1(&self.0, major, minor, permissions.as_ref()).await?;
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
            container.wait().await.ok();
            handle.abort();
            Ok::<(), Error>(())
        })
    }
}

async fn allow_device_cgroup1(
    container_id: &str,
    major: u64,
    minor: u64,
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
    major: u64,
    minor: u64,
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
