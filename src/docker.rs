use anyhow::{ensure, Context, Result};

use std::ops::DerefMut;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::spawn;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

use raw_tty::GuardMode;

use bollard::errors::Error;

pub use bollard::container::LogOutput;
pub use bollard::models::RestartPolicyNameEnum as RestartPolicy;

use crate::tokio_ext::WithJoinHandleGuard;

pub struct Docker(bollard::Docker);

#[derive(Clone)]
pub struct Container(String, bollard::Docker);

pub struct IoStream {
    pub output: std::pin::Pin<
        std::boxed::Box<dyn futures_core::stream::Stream<Item = Result<LogOutput, Error>> + Send>,
    >,
    pub input: std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
}

impl Docker {
    pub fn connect_with_defaults() -> Result<Docker> {
        Ok(Docker(bollard::Docker::connect_with_local_defaults()?))
    }

    pub async fn get_container<T: AsRef<str>>(&self, name: T) -> Result<Container> {
        let response = self.0.inspect_container(name.as_ref(), None).await?;
        Ok(Container(
            response.id.context("Failed to obtain container ID")?,
            self.0.clone(),
        ))
    }

    pub async fn run<U: AsRef<str>, T: AsRef<[U]>>(&self, args: T) -> Result<Container> {
        let args = args.as_ref().iter().map(|arg| arg.as_ref());
        let args = ["run", "-d", "--rm", "--restart=no"]
            .into_iter()
            .chain(args);

        let output = tokio::process::Command::new("docker")
            .args(args)
            .stdout(std::process::Stdio::piped())
            .spawn()?
            .wait_with_output()
            .await?;

        ensure!(
            output.status.success(),
            "Failed to create container: {}",
            String::from_utf8_lossy(output.stderr.as_slice())
        );

        let id = String::from_utf8(output.stdout)?;
        self.get_container(id.trim()).await
    }
}

impl Container {
    pub fn id(&self) -> &str {
        &self.0
    }

    pub async fn remove(self, force: bool) -> Result<()> {
        let options = bollard::container::RemoveContainerOptions {
            force,
            ..Default::default()
        };
        self.1.remove_container(&self.0, Some(options)).await?;
        Ok(())
    }

    pub async fn start(&self) -> Result<()> {
        self.1.start_container::<String>(&self.0, None).await?;
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

        let options = bollard::exec::StartExecOptions {
            detach: false,
            ..Default::default()
        };
        let response = self.1.start_exec(&response.id, Some(options)).await?;

        if let bollard::exec::StartExecResults::Attached { input, output } = response {
            return Ok(IoStream { output, input });
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
            detach_keys: Some("ctrl-c".to_string()),
        };

        let response = self.1.attach_container(&self.0, Some(options)).await?;

        Ok(IoStream {
            output: response.output,
            input: response.input,
        })
    }

    async fn inspect(&self) -> Result<bollard::models::ContainerInspectResponse> {
        Ok(self.1.inspect_container(self.0.as_ref(), None).await?)
    }

    pub async fn running(&self) -> Result<bool> {
        let inspect = self.inspect().await?;
        let state = inspect.state.context("Failed to obtain container state")?;
        Ok(state.running.unwrap_or(false))
    }

    pub async fn ensure_running(&self) -> Result<()> {
        if !self.running().await? {
            self.start().await?;
        }
        Ok(())
    }

    pub async fn name(&self) -> Result<String> {
        let inspect = self.inspect().await?;
        let name = inspect.name.context("Failed to obtain container name")?;
        Ok(name)
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
        self.rm(&link).await?;
        self.mkdir_for(&link).await?;
        self.exec([
            "ln",
            "-s",
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

impl IoStream {
    pub async fn collect(mut self) -> Result<String> {
        let mut result = String::default();
        while let Some(output) = self.output.next().await {
            result.push_str(&output?.to_string());
        }
        return Ok(result);
    }

    pub fn pipe_std(self) -> Result<JoinHandle<Result<()>>> {
        let mut stdin = tokio_fd::AsyncFd::try_from(libc::STDIN_FILENO)?.guard_mode()?;
        stdin.set_raw_mode()?;
        let stdout = tokio_fd::AsyncFd::try_from(libc::STDOUT_FILENO)?;
        let stderr = tokio_fd::AsyncFd::try_from(libc::STDERR_FILENO)?;
        Ok(self.pipe(stdin, stdout, stderr))
    }

    pub fn pipe<I, II, O, E>(
        self,
        mut stdin: II,
        mut stdout: O,
        mut stderr: E,
    ) -> JoinHandle<Result<()>>
    where
        I: tokio::io::AsyncRead + std::marker::Unpin + Send + 'static,
        II: DerefMut<Target = I> + std::marker::Unpin + Send + 'static,
        O: tokio::io::AsyncWrite + std::marker::Unpin + Send + 'static,
        E: tokio::io::AsyncWrite + std::marker::Unpin + Send + 'static,
    {
        let mut input = self.input;
        let mut output = self.output;

        spawn(async move {
            let _stdin_guarg = spawn(async move {
                let mut buf = bytes::BytesMut::with_capacity(1024);
                while let Ok(_) = stdin.read_buf(&mut buf).await {
                    input.write_all_buf(&mut buf).await?;
                }
                return Ok::<(), anyhow::Error>(());
            })
            .guard();

            while let Some(output) = output.next().await {
                match output? {
                    LogOutput::Console { mut message } => {
                        stdout.write_all_buf(&mut message).await?
                    }
                    LogOutput::StdOut { mut message } => stdout.write_all_buf(&mut message).await?,
                    LogOutput::StdErr { mut message } => stderr.write_all_buf(&mut message).await?,
                    _ => continue,
                };
            }

            return Ok::<(), anyhow::Error>(());
        })
    }
}
