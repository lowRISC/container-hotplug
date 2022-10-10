use crate::tokio_ext::WithJoinHandleGuard;

use anyhow::{Context, Result};
use bollard::container::LogOutput;
use bollard::errors::Error;
use raw_tty::GuardMode;
use std::ops::DerefMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};
use tokio::spawn;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

pub(super) enum IoStreamSource {
    Container(String),
    Exec(String),
}

pub struct IoStream {
    pub output: std::pin::Pin<
        std::boxed::Box<dyn futures_core::stream::Stream<Item = Result<LogOutput, Error>> + Send>,
    >,
    pub input: std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
    pub(super) source: IoStreamSource,
    pub(super) docker: bollard::Docker,
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
        let stdout = tokio_fd::AsyncFd::try_from(libc::STDOUT_FILENO)?.guard_mode()?;
        let stderr = tokio_fd::AsyncFd::try_from(libc::STDERR_FILENO)?.guard_mode()?;
        stdin.modify_mode(|mut t| {
            use libc::*;
            t.c_iflag &= !(IGNBRK | BRKINT | PARMRK | ISTRIP | INLCR | IGNCR | ICRNL | IXON);
            t.c_lflag &= !(ECHO | ECHONL | ICANON | ISIG | IEXTEN);
            t.c_cflag &= !(CSIZE | PARENB);
            t.c_cflag |= CS8;
            t
        })?;
        let resize_stream = async_stream::try_stream! {
            let mut stream = signal(SignalKind::window_change())?;
            loop {
                let size = termsize::get().context("Failed to obtain tty size")?;
                yield (size.rows, size.cols);
                stream.recv().await;
            }
        };

        Ok(self.pipe(stdin, stdout, stderr, resize_stream))
    }

    pub fn pipe<I, II, O, OO, E, EE>(
        self,
        mut stdin: II,
        mut stdout: OO,
        mut stderr: EE,
        resize_stream: impl tokio_stream::Stream<Item = Result<(u16, u16)>> + Send + 'static,
    ) -> JoinHandle<Result<()>>
    where
        I: tokio::io::AsyncRead + std::marker::Unpin + Send + 'static,
        II: DerefMut<Target = I> + std::marker::Unpin + Send + 'static,
        O: tokio::io::AsyncWrite + std::marker::Unpin + Send + 'static,
        OO: DerefMut<Target = O> + std::marker::Unpin + Send + 'static,
        E: tokio::io::AsyncWrite + std::marker::Unpin + Send + 'static,
        EE: DerefMut<Target = E> + std::marker::Unpin + Send + 'static,
    {
        let mut input = self.input;
        let mut output = self.output;
        let docker = self.docker;
        let source = self.source;

        spawn(async move {
            let _resize_guard = spawn(async move {
                tokio::pin!(resize_stream);
                while let Some(size) = resize_stream.next().await {
                    resize_tty(&docker, &source, size?).await?;
                }
                return Ok::<(), anyhow::Error>(());
            })
            .guard();

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
                        stdout.write_all_buf(&mut message).await?;
                    }
                    LogOutput::StdOut { mut message } => {
                        stdout.write_all_buf(&mut message).await?;
                    }
                    LogOutput::StdErr { mut message } => {
                        stderr.write_all_buf(&mut message).await?;
                    }
                    _ => continue,
                };
            }

            return Ok::<(), anyhow::Error>(());
        })
    }
}

async fn resize_tty(
    docker: &bollard::Docker,
    source: &IoStreamSource,
    (rows, cols): (u16, u16),
) -> Result<()> {
    match source {
        IoStreamSource::Container(id) => {
            let options = bollard::container::ResizeContainerTtyOptions {
                height: rows,
                width: cols,
            };
            docker.resize_container_tty(&id, options).await?;
        }
        IoStreamSource::Exec(id) => {
            let options = bollard::exec::ResizeExecOptions {
                height: rows,
                width: cols,
            };
            docker.resize_exec(&id, options).await?;
        }
    };
    Ok(())
}
