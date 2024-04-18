use anyhow::{Context, Result};
use async_stream::try_stream;
use bollard::container::LogOutput;
use bollard::errors::Error;
use bytes::Bytes;
use std::pin::{pin, Pin};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;
use tokio_stream::{Stream, StreamExt};
use tokio_util::io::ReaderStream;

pub struct IoStream {
    pub output: std::pin::Pin<Box<dyn Stream<Item = Result<LogOutput, Error>> + Send>>,
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
    pub(super) id: String,
    pub(super) docker: bollard::Docker,
}

enum StreamData {
    Resize(u16, u16),
    StdIn(Bytes),
    StdOut(Bytes),
    StdErr(Bytes),
}

impl IoStream {
    pub fn pipe_std(self) -> JoinHandle<Result<()>> {
        let stdin = crate::util::tty_mode_guard::TtyModeGuard::new(tokio::io::stdin(), |mode| {
            // Switch input to raw mode, but don't touch output modes (as it can also be connected
            // to stdout and stderr).
            let outmode = mode.output_modes;
            mode.make_raw();
            mode.output_modes = outmode;
        });
        let stdout = tokio::io::stdout();
        let stderr = tokio::io::stderr();

        let resize_stream = try_stream! {
            let mut stream = signal(SignalKind::window_change())?;
            loop {
                match rustix::termios::tcgetwinsize(rustix::stdio::stdout()) {
                    Ok(size) => yield (size.ws_row, size.ws_col),
                    _ => {},
                }
                stream.recv().await;
            }
        };

        self.pipe(stdin, stdout, stderr, resize_stream)
    }

    pub fn pipe(
        self,
        stdin: impl AsyncRead + Send + 'static,
        stdout: impl AsyncWrite + Send + 'static,
        stderr: impl AsyncWrite + Send + 'static,
        resize_stream: impl Stream<Item = std::io::Result<(u16, u16)>> + Send + 'static,
    ) -> JoinHandle<Result<()>> {
        let mut input = self.input;
        let docker = self.docker;
        let id = self.id;

        let resize_stream = resize_stream.map(|data| {
            let (rows, cols) = data.context("Listening for tty resize")?;
            Ok(StreamData::Resize(rows, cols))
        });

        let input_stream = ReaderStream::new(stdin).map(|data| {
            Ok(StreamData::StdIn(
                data.context("Reading container input stream")?,
            ))
        });

        let output_stream = self.output.filter_map(|output| match output {
            Ok(LogOutput::Console { message }) => Some(Ok(StreamData::StdOut(message))),
            Ok(LogOutput::StdOut { message }) => Some(Ok(StreamData::StdOut(message))),
            Ok(LogOutput::StdErr { message }) => Some(Ok(StreamData::StdErr(message))),
            Err(err) => Some(Err(err).context("Reading container output stream")),
            _ => None,
        });

        tokio::spawn(async move {
            let mut streams = pin!(resize_stream.merge(input_stream).merge(output_stream));
            let mut stdout = pin!(stdout);
            let mut stderr = pin!(stderr);

            while let Some(data) = streams.next().await {
                match data? {
                    StreamData::Resize(rows, cols) => {
                        resize_tty(&docker, &id, (rows, cols)).await?;
                    }
                    StreamData::StdIn(mut buf) => {
                        input.write_all_buf(&mut buf).await?;
                        input.flush().await?;
                    }
                    StreamData::StdOut(mut buf) => {
                        stdout.write_all_buf(&mut buf).await?;
                        stdout.flush().await?;
                    }
                    StreamData::StdErr(mut buf) => {
                        stderr.write_all_buf(&mut buf).await?;
                        stdout.flush().await?;
                    }
                };
            }

            Ok(())
        })
    }
}

async fn resize_tty(docker: &bollard::Docker, id: &str, (rows, cols): (u16, u16)) -> Result<()> {
    let options = bollard::container::ResizeContainerTtyOptions {
        height: rows,
        width: cols,
    };
    docker.resize_container_tty(id, options).await?;
    Ok(())
}
