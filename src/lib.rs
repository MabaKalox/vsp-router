use bytes::Bytes;
use camino::{Utf8Path, Utf8PathBuf};
use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use std::fs;
use std::sync::Arc;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::broadcast,
    sync::broadcast::error::RecvError,
    sync::Mutex,
};
use tokio_serial::{SerialPort, SerialPortBuilderExt, SerialStream};
use tokio_util::io::ReaderStream;
use tracing::error;

#[cfg(unix)]
use std::os::unix;

#[derive(Error, Debug)]
pub enum Error {
    #[error("could not create link to pty")]
    Link(#[source] std::io::Error),

    #[error("serial error")]
    Serial(#[source] tokio_serial::Error),

    #[error("stream closed")]
    Closed,

    #[error("read error")]
    Read(#[source] std::io::Error),

    #[error("write error")]
    Write(#[source] std::io::Error),
}

pub struct PtyLink {
    // Not used directly but need to keep around to prevent early close of the file descriptor.
    //
    // tokio_serial::SerialStream includes a mio_serial::SerialStream which includes a
    // serialport::TTY which includes a Drop impl that closes the file descriptor.
    _subordinate: SerialStream,
    link: Utf8PathBuf,
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(unix)]
pub fn create_virtual_serial_port<P>(path: P) -> Result<(SerialStream, PtyLink)>
where
    P: AsRef<Utf8Path>,
{
    let (manager, subordinate) = SerialStream::pair().map_err(Error::Serial)?;
    let link = PtyLink::new(subordinate, path)?;

    Ok((manager, link))
}

pub fn open_physical_serial_port<P>(path: P, baud_rate: u32) -> Result<SerialStream>
where
    P: AsRef<Utf8Path>,
{
    tokio_serial::new(path.as_ref().as_str(), baud_rate)
        .open_native_async()
        .map_err(Error::Serial)
}

#[cfg(unix)]
impl PtyLink {
    fn new<P: AsRef<Utf8Path>>(subordinate: SerialStream, path: P) -> Result<Self> {
        let link = path.as_ref().to_path_buf();
        unix::fs::symlink(subordinate.name().unwrap(), link.as_std_path()).map_err(Error::Link)?;

        Ok(PtyLink {
            _subordinate: subordinate,
            link,
        })
    }

    pub fn link(&self) -> &Utf8Path {
        self.link.as_path()
    }

    pub fn id(&self) -> &str {
        self.link.as_str()
    }
}

impl Drop for PtyLink {
    fn drop(&mut self) {
        if fs::remove_file(&self.link).is_err() {
            eprintln!("error: could not delete {}", self.link);
        }
    }
}

pub fn handle_send<W>(
    mut receiver: broadcast::Receiver<Bytes>,
    sink: Arc<Sink<W>>,
) -> BoxFuture<'static, Result<()>>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    Box::pin(async move {
        loop {
            let packet = match receiver.recv().await {
                Ok(bytes) => bytes,
                Err(RecvError::Lagged(n)) => {
                    format!("\n...Dropped {} packets of bytes...\n", n).into()
                }
                Err(RecvError::Closed) => break,
            };

            sink.send(packet).await?;
        }
        Ok(())
    })
}

pub fn handle_receive<R>(
    mut r: ReaderStream<R>,
    sender: broadcast::Sender<Bytes>,
) -> BoxFuture<'static, Result<()>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    Box::pin(async move {
        while let Some(result) = r.next().await {
            let bytes = result.map_err(Error::Read)?;
            let _ = sender.send(bytes);
        }

        Ok(())
    })
}

pub struct Sink<W>(Mutex<W>);

impl<W> Sink<W>
where
    W: AsyncWrite + Unpin,
{
    pub fn new(w: W) -> Self {
        Self(Mutex::new(w))
    }

    pub async fn send(&self, data: Bytes) -> Result<()> {
        let mut guard = self.0.lock().await;
        guard.write_all(&data).await.map_err(Error::Write)?;
        guard.flush().await.map_err(Error::Write)?;
        Ok(())
    }
}
