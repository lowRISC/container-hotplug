use super::Container;

use anyhow::{ensure, Context, Result};

pub struct Docker(bollard::Docker);

impl Docker {
    pub fn connect_with_defaults() -> Result<Docker> {
        Ok(Docker(bollard::Docker::connect_with_local_defaults()?))
    }

    pub async fn get<T: AsRef<str>>(&self, name: T) -> Result<Container> {
        let response = self.0.inspect_container(name.as_ref(), None).await?;
        let id = response.id.context("Failed to obtain container ID")?;
        let config = response
            .config
            .context("Failed to obtain container config")?;
        let user = config.user.context("Failed to obtain container user")?;
        Container::new(&self.0, id, user)
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
        self.get(id.trim()).await
    }
}
