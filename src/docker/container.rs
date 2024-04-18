use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Error, Result};
use bollard::service::EventMessage;
use futures::future::{BoxFuture, Shared};
use futures::FutureExt;
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::{spawn, JoinHandle};
use tokio_stream::StreamExt;

use super::{IoStream, IoStreamSource};
use crate::cgroup::{
    Access, DeviceAccessController, DeviceAccessControllerDummy, DeviceAccessControllerV1,
    DeviceAccessControllerV2, DeviceType,
};

#[derive(Clone)]
pub struct Container {
    docker: bollard::Docker,
    id: String,
    user: String,
    remove_event: Shared<BoxFuture<'static, Option<EventMessage>>>,
    cgroup_device_filter: Arc<Mutex<Option<Box<dyn DeviceAccessController + Send>>>>,
}

impl Container {
    pub(super) fn new(docker: &bollard::Docker, id: String, user: String) -> Result<Self> {
        let mut remove_events = docker.events(Some(bollard::system::EventsOptions {
            filters: [
                ("container".to_owned(), vec![id.to_owned()]),
                ("type".to_owned(), vec!["container".to_owned()]),
                ("event".to_owned(), vec!["destroy".to_owned()]),
            ]
            .into(),
            ..Default::default()
        }));

        // Spawn the future to start listening event.
        let remove_evevnt = tokio::spawn(async move { remove_events.next().await?.ok() })
            .map(|x| x.ok().flatten())
            .boxed()
            .shared();

        let cgroup_device_filter: Box<dyn DeviceAccessController + Send> =
            match DeviceAccessControllerV2::new(
                format!("/sys/fs/cgroup/system.slice/docker-{id}.scope").as_ref(),
            ) {
                Ok(v) => Box::new(v),
                Err(err2) => match DeviceAccessControllerV1::new(
                    format!("/sys/fs/cgroup/devices/docker/{id}").as_ref(),
                ) {
                    Ok(v) => Box::new(v),
                    Err(err1) => {
                        log::error!("neither cgroup v1 and cgroup v2 works");
                        log::error!("cgroup v2: {err2}");
                        log::error!("cgroup v1: {err1}");
                        Box::new(DeviceAccessControllerDummy)
                    }
                },
            };

        Ok(Self {
            docker: docker.clone(),
            id,
            user: if user.is_empty() {
                // If user is not specified, use root.
                "root".to_owned()
            } else {
                user
            },
            remove_event: remove_evevnt,
            cgroup_device_filter: Arc::new(Mutex::new(Some(cgroup_device_filter))),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub async fn remove(&self, timeout: Option<Duration>) -> Result<()> {
        log::info!("Removing container {}", self.id);

        // Since we passed "--rm" flag, docker will automatically start removing the container.
        // Ignore any error for manual removal.
        let _: Result<()> = async {
            self.rename(&format!("removing-{}", self.id)).await?;
            let options = bollard::container::RemoveContainerOptions {
                force: true,
                ..Default::default()
            };
            self.docker
                .remove_container(&self.id, Some(options))
                .await?;
            Ok(())
        }
        .await;

        if let Some(duration) = timeout {
            tokio::time::timeout(duration, self.remove_event.clone())
                .await?
                .context("no destroy event")?;
        } else {
            self.remove_event
                .clone()
                .await
                .context("no destroy event")?;
        }

        // Stop the cgroup device filter. Only do so once we're sure that the container is removed.
        self.cgroup_device_filter
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .stop()?;

        Ok(())
    }

    pub async fn rename(&self, name: &str) -> Result<()> {
        let required = bollard::container::RenameContainerOptions { name };
        self.docker.rename_container(&self.id, required).await?;
        Ok(())
    }

    pub async fn exec_as_root<T: ToString>(&self, cmd: &[T]) -> Result<IoStream> {
        let cmd = cmd.iter().map(|s| s.to_string()).collect();
        let options = bollard::exec::CreateExecOptions {
            cmd: Some(cmd),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            tty: Some(true),
            detach_keys: Some("ctrl-c".to_string()),
            user: Some("root".to_string()),
            ..Default::default()
        };
        let response = self.docker.create_exec::<String>(&self.id, options).await?;
        let id = response.id;

        let options = bollard::exec::StartExecOptions {
            detach: false,
            ..Default::default()
        };
        let response = self.docker.start_exec(&id, Some(options)).await?;
        let bollard::exec::StartExecResults::Attached { input, output } = response else {
            unreachable!("we asked for attached IO streams");
        };

        Ok(IoStream {
            output,
            input,
            source: IoStreamSource::Exec(id),
            docker: self.docker.clone(),
        })
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

    pub async fn name(&self) -> Result<String> {
        let inspect = self
            .docker
            .inspect_container(self.id.as_ref(), None)
            .await?;
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

    pub async fn chown_to_user(&self, path: &str) -> Result<()> {
        // Use `-h` to not follow symlink, and `user:` will use user's login group.
        self.exec_as_root(&["chown", "-h", &format!("{}:", self.user), path])
            .await?
            .collect()
            .await?;
        Ok(())
    }

    // Note: we use `&str` here instead of `Path` because docker API expects string instead `OsStr`.
    pub async fn mkdir(&self, path: &str) -> Result<()> {
        self.exec_as_root(&["mkdir", "-p", path])
            .await?
            .collect()
            .await?;
        Ok(())
    }

    pub async fn mkdir_for(&self, path: &str) -> Result<()> {
        if let Some(path) = std::path::Path::new(path).parent() {
            self.mkdir(path.to_str().unwrap()).await?;
        }
        Ok(())
    }

    pub async fn mknod(&self, node: &str, (major, minor): (u32, u32)) -> Result<()> {
        self.rm(node).await?;
        self.mkdir_for(node).await?;
        self.exec_as_root(&["mknod", node, "c", &major.to_string(), &minor.to_string()])
            .await?
            .collect()
            .await?;
        self.chown_to_user(node).await?;
        Ok(())
    }

    pub async fn symlink(&self, source: &str, link: &str) -> Result<()> {
        self.mkdir_for(link).await?;
        self.exec_as_root(&["ln", "-sf", source, link])
            .await?
            .collect()
            .await?;
        self.chown_to_user(link).await?;
        Ok(())
    }

    pub async fn rm(&self, node: &str) -> Result<()> {
        self.exec_as_root(&["rm", "-f", node])
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
        let controller = self.cgroup_device_filter.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut controller = controller.lock().unwrap();
            controller.as_mut().unwrap().set_permission(
                DeviceType::Character,
                major,
                minor,
                if r { Access::READ } else { Access::empty() }
                    | if w { Access::WRITE } else { Access::empty() }
                    | if m { Access::MKNOD } else { Access::empty() },
            )?;

            Ok(())
        })
        .await?
    }

    pub async fn pipe_signals(&self) -> JoinHandle<Result<()>> {
        let container = self.clone();
        let signal_handler = async move {
            let mut stream = pin!(signal_stream(SignalKind::alarm())
                .merge(signal_stream(SignalKind::hangup()))
                .merge(signal_stream(SignalKind::interrupt()))
                .merge(signal_stream(SignalKind::quit()))
                .merge(signal_stream(SignalKind::terminate()))
                .merge(signal_stream(SignalKind::user_defined1()))
                .merge(signal_stream(SignalKind::user_defined2())));

            while let Some(signal) = stream.next().await {
                container.kill(signal?.as_raw_value()).await?;
            }

            Err::<_, Error>(anyhow!("Failed to listen for signals"))
        };

        let container = self.clone();
        let wait_for_exit = async move { container.wait().await };

        spawn(async move {
            tokio::select! {
                result = signal_handler => result,
                result = wait_for_exit => result,
            }?;
            Ok(())
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
