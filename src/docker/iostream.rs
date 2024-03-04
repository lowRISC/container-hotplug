use anyhow::{Context, Result};
use async_stream::try_stream;
use bollard::container::LogOutput;
use bollard::errors::Error;
use bytes::Bytes;
use std::io;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;
use tokio_stream::{empty, Stream, StreamExt};
use tokio_util::io::ReaderStream;

pub(super) enum IoStreamSource {
    Container(String),
    Exec(String),
}

pub struct IoStream {
    pub output: std::pin::Pin<Box<dyn Stream<Item = Result<LogOutput, Error>> + Send>>,
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
    pub(super) source: IoStreamSource,
    pub(super) docker: bollard::Docker,
}

enum StreamData {
    Resize(u16, u16),
    StdIn(Bytes),
    StdOut(Bytes),
    StdErr(Bytes),
    Stop,
}

impl IoStream {
    pub async fn collect(mut self) -> Result<String> {
        let mut result = String::default();
        while let Some(output) = self.output.next().await {
            result.push_str(&output?.to_string());
        }
        Ok(result)
    }

    pub fn pipe_std(self) -> JoinHandle<()> {
        let stdin = Box::pin(crate::util::tty_mode_guard::TtyModeGuard::new(
            tokio::io::stdin(),
            |mode| {
                // Switch input to raw mode, but don't touch output modes (as it can also be connected
                // to stdout and stderr).
                let outmode = mode.output_modes;
                mode.make_raw();
                mode.output_modes = outmode;
            },
        ));
        let stdout = Box::pin(tokio::io::stdout());
        let stderr = Box::pin(tokio::io::stderr());

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
        stdin: Pin<Box<dyn AsyncRead + Send + 'static>>,
        mut stdout: Pin<Box<dyn AsyncWrite + Send + 'static>>,
        mut stderr: Pin<Box<dyn AsyncWrite + Send + 'static>>,
        resize_stream: impl Stream<Item = io::Result<(u16, u16)>> + Send + 'static,
    ) -> JoinHandle<()> {
        let mut input = self.input;
        let mut output = self.output;
        let docker = self.docker;
        let source = self.source;

        let resize_stream = async_stream::stream! {
            tokio::pin!(resize_stream);
            while let Some(data) = resize_stream.next().await {
                yield match data {
                    Ok((rows, cols)) => Ok(StreamData::Resize(rows, cols)),
                    Err(err) => Err(err).context("Listening for tty resize"),
                };
            }
        };

        let input_stream = async_stream::stream! {
            let mut stdin = ReaderStream::new(stdin);
            while let Some(data) = stdin.next().await {
                yield match data {
                    Ok(buf) => Ok(StreamData::StdIn(buf)),
                    Err(err) => Err(err).context("Reading container input stream"),
                };
            }
        };

        let output_stream = async_stream::stream! {
            while let Some(output) = output.next().await {
                yield match output {
                    Ok(LogOutput::Console{message}) => Ok(StreamData::StdOut(message)),
                    Ok(LogOutput::StdOut{message}) => Ok(StreamData::StdOut(message)),
                    Ok(LogOutput::StdErr{message}) => Ok(StreamData::StdErr(message)),
                    Err(err) => Err(err).context("Reading container output stream"),
                    _ => continue,
                };
            }
            yield Ok(StreamData::Stop);
        };

        let streams = empty()
            .merge(resize_stream)
            .merge(input_stream)
            .merge(output_stream);

        let stream = async_stream::try_stream! {
            tokio::pin!(streams);
            while let Some(data) = streams.next().await {
                match data? {
                    StreamData::Resize(rows, cols) => {
                        resize_tty(&docker, &source, (rows, cols)).await?;
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
                    StreamData::Stop => {
                        break
                    }
                };
                yield ();
            }
        };

        tokio::spawn(async move {
            tokio::pin!(stream);
            let _ = stream.all(|_: Result<()>| true).await;
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
            docker.resize_container_tty(id, options).await?;
        }
        IoStreamSource::Exec(id) => {
            let options = bollard::exec::ResizeExecOptions {
                height: rows,
                width: cols,
            };
            docker.resize_exec(id, options).await?;
        }
    };
    Ok(())
}
