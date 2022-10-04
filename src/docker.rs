use anyhow::{Context, Result};

use tokio::io::AsyncWriteExt;
use tokio_stream::StreamExt;

use bollard::errors::Error;

pub use bollard::container::LogOutput;
pub use bollard::models::RestartPolicyNameEnum as RestartPolicy;

pub struct Docker(bollard::Docker);

#[derive(Clone)]
pub struct Container(String, bollard::Docker);

pub struct IoStream {
    pub output: std::pin::Pin<
        std::boxed::Box<dyn futures_core::stream::Stream<Item = Result<LogOutput, Error>> + Send>,
    >,
    pub input: std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
}

pub struct ContainerBuilder {
    docker: bollard::Docker,
    image: String,
    name: Option<String>,
    binds: Option<Vec<String>>,
    restart_policy: Option<RestartPolicy>,
    cmd: Option<Vec<String>>,
    auto_remove: Option<bool>,
    remove_old: bool,
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

    pub fn with_image<T: Into<String>>(&self, image: T) -> ContainerBuilder {
        ContainerBuilder {
            docker: self.0.clone(),
            image: image.into(),
            name: None,
            binds: None,
            restart_policy: None,
            cmd: None,
            auto_remove: None,
            remove_old: false,
        }
    }
}

impl ContainerBuilder {
    pub fn name<T: AsRef<str>>(&mut self, name: T) -> &mut Self {
        self.name = Some(name.as_ref().into());
        self
    }

    pub fn bind<U: AsRef<str>, T: AsRef<[U]>>(&mut self, binds: T) -> &mut Self {
        let iter = binds.as_ref().iter().map(|s| s.as_ref().into());
        match &mut self.binds {
            Some(binds) => binds.extend(iter),
            None => self.binds = Some(iter.collect()),
        };
        self
    }

    pub fn restart_policy<T: Into<RestartPolicy>>(&mut self, restart_policy: T) -> &mut Self {
        self.restart_policy = Some(restart_policy.into());
        self
    }

    pub fn cmd<U: AsRef<str>, T: AsRef<[U]>>(&mut self, devices: T) -> &mut Self {
        let iter = devices.as_ref().iter().map(|s| s.as_ref().into());
        self.cmd = Some(iter.collect());
        self
    }

    pub fn bash<T: AsRef<str>>(&mut self, cmd: T) -> &mut Self {
        self.cmd(["/bin/bash", "-c", cmd.as_ref()])
    }

    pub fn auto_remove(&mut self, auto_remove: bool) -> &mut Self {
        self.auto_remove = Some(auto_remove);
        self
    }

    pub fn remove_old(&mut self, remove_old: bool) -> &mut Self {
        self.remove_old = remove_old;
        self
    }

    pub async fn create(&self) -> Result<Container> {
        let mut options = None;
        let mut config: bollard::container::Config<String> = Default::default();
        let mut host_config: bollard::models::HostConfig = Default::default();

        config.image = Some(self.image.clone());
        config.cmd = self.cmd.clone();
        config.tty = Some(true);

        if let Some(name) = &self.name {
            options = Some(bollard::container::CreateContainerOptions { name });

            if self.remove_old {
                let docker = Docker(self.docker.clone());
                if let Some(container) = docker.get_container(&name).await.ok() {
                    container.remove(true).await?;
                }
            }
        }

        host_config.binds = self.binds.clone();
        host_config.auto_remove = self.auto_remove.clone();

        if let Some(restart_policy) = self.restart_policy {
            host_config.restart_policy = Some(bollard::models::RestartPolicy {
                name: Some(restart_policy),
                ..Default::default()
            });
        }

        Some(bollard::models::RestartPolicy {
            name: Some(bollard::models::RestartPolicyNameEnum::NO),
            ..Default::default()
        });

        config.host_config = Some(host_config);

        let response = self.docker.create_container(options, config).await?;

        Ok(Container(response.id, self.docker.clone()))
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

    pub async fn bash<T: AsRef<str>>(&self, cmd: T) -> Result<IoStream> {
        self.exec(["/bin/bash", "-c", cmd.as_ref()]).await
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
        self.exec(["/usr/bin/mkdir", "-p", &path.as_ref().to_string_lossy()])
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
            "/usr/bin/mknod",
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
            "/usr/bin/ln",
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
        self.exec(["/usr/bin/rm", "-f", &node.as_ref().to_string_lossy()])
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
    file.write(format!("c {major}:{minor} {permissions}").as_bytes())
        .await?;
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
    file.write(format!("c {major}:{minor} {permissions}").as_bytes())
        .await?;
    Ok(())
}

impl IoStream {
    pub async fn collect(&mut self) -> Result<String> {
        let mut result = String::default();
        while let Some(output) = self.output.next().await {
            result.push_str(&output?.to_string());
        }
        return Ok(result);
    }

    pub async fn pipe_std(&mut self) -> Result<()> {
        self.pipe(&mut tokio::io::stdout(), &mut tokio::io::stderr())
            .await?;
        Ok(())
    }

    pub async fn pipe<O, E>(&mut self, stdout: &mut O, stderr: &mut E) -> Result<()>
    where
        O: tokio::io::AsyncWrite + std::marker::Unpin,
        E: tokio::io::AsyncWrite + std::marker::Unpin,
    {
        while let Some(output) = self.output.next().await {
            match output? {
                LogOutput::Console { message } => stdout.write(message.as_ref()).await?,
                LogOutput::StdOut { message } => stdout.write(message.as_ref()).await?,
                LogOutput::StdErr { message } => stderr.write(message.as_ref()).await?,
                _ => continue,
            };
        }
        return Ok(());
    }
}
