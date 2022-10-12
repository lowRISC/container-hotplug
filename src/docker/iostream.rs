use anyhow::{Context, Result};
use async_stream::try_stream;
use bollard::container::LogOutput;
use bollard::errors::Error;
use bytes::Bytes;
use raw_tty::GuardMode;
use std::future::pending;
use std::io;
use std::pin::Pin;
use tokio::io::{sink, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;
use tokio_stream::{empty, Stream, StreamExt};
use tokio_util::io::{ReaderStream, StreamReader};

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
        return Ok(result);
    }

    pub fn pipe_std(self) -> JoinHandle<()> {
        let stdin = stdin();
        let stdout = stdout();
        let stderr = stderr();

        let resize_stream = try_stream! {
            let mut stream = signal(SignalKind::window_change())?;
            loop {
                match termsize::get().ok_or(io::Error::from_raw_os_error(libc::ENOTTY)) {
                    Ok(size) => yield (size.rows, size.cols),
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
                    }
                    StreamData::StdOut(mut buf) => {
                        stdout.write_all_buf(&mut buf).await?;
                    }
                    StreamData::StdErr(mut buf) => {
                        stderr.write_all_buf(&mut buf).await?;
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
            let _ = stream.all(|_: Result<(), anyhow::Error>| true).await;
        })
    }
}

fn stdin() -> Pin<Box<dyn tokio::io::AsyncRead + Send>> {
    if let Ok(stdin) = try_stdin() {
        stdin
    } else {
        let stream = async_stream::stream! {
            yield pending::<std::io::Result<bytes::BytesMut>>().await;
        };
        Box::pin(StreamReader::new(stream))
    }
}

fn try_stdin() -> Result<Pin<Box<dyn AsyncRead + Send>>> {
    let mut stdin = tokio_fd::AsyncFd::try_from(libc::STDIN_FILENO)?.guard_mode()?;
    stdin.modify_mode(|mut t| {
        use libc::*;
        t.c_iflag &= !(IGNBRK | BRKINT | PARMRK | ISTRIP | INLCR | IGNCR | ICRNL | IXON);
        t.c_lflag &= !(ECHO | ECHONL | ICANON | ISIG | IEXTEN);
        t.c_cflag &= !(CSIZE | PARENB);
        t.c_cflag |= CS8;
        t
    })?;
    let stream = async_stream::stream! {
        loop {
            let mut buf = bytes::BytesMut::with_capacity(1024);
            match stdin.read_buf(&mut buf).await {
                Ok(_) => yield Ok(buf),
                Err(err) => yield Err(err),
            }
        }
    };
    Ok(Box::pin(StreamReader::new(stream)))
}

fn stdout() -> Pin<Box<dyn tokio::io::AsyncWrite + Send>> {
    match tokio_fd::AsyncFd::try_from(libc::STDOUT_FILENO) {
        Ok(stdout) => Box::pin(stdout),
        Err(_) => Box::pin(sink()),
    }
}

fn stderr() -> Pin<Box<dyn tokio::io::AsyncWrite + Send>> {
    match tokio_fd::AsyncFd::try_from(libc::STDERR_FILENO) {
        Ok(stdout) => Box::pin(stdout),
        Err(_) => Box::pin(sink()),
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
