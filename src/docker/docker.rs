use super::Container;

use anyhow::{ensure, Context, Result};
use bollard::service::EventMessage;
use futures::{
    future::{BoxFuture, Shared},
    FutureExt, StreamExt,
};

pub struct Docker(bollard::Docker);

impl Docker {
    pub fn connect_with_defaults() -> Result<Docker> {
        Ok(Docker(bollard::Docker::connect_with_local_defaults()?))
    }

    pub async fn get_container<T: AsRef<str>>(&self, name: T) -> Result<Container> {
        let response = self.0.inspect_container(name.as_ref(), None).await?;
        let id = response.id.context("Failed to obtain container ID")?;
        Ok(Container(
            id.clone(),
            self.0.clone(),
            container_removed_future(&self.0, id.clone()),
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

fn container_removed_future(
    docker: &bollard::Docker,
    id: String,
) -> Shared<BoxFuture<'static, Option<EventMessage>>> {
    let options = bollard::system::EventsOptions {
        filters: [
            (String::from("container"), vec![id.clone()]),
            (String::from("type"), vec![String::from("container")]),
            (String::from("event"), vec![String::from("destroy")]),
        ]
        .into(),
        ..Default::default()
    };

    let removed = docker
        .events(Some(options))
        .map(|evt| evt.ok())
        .take(1)
        .collect::<Vec<_>>()
        .map(|vec| vec.into_iter().next().flatten())
        .boxed()
        .shared();

    removed
}
