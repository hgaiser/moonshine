use reis::{ei, tokio::EiConvertEventStream};
use std::io;
use futures::stream::StreamExt;

pub struct ReisClient {
    stream: EiConvertEventStream,
    connection: reis::event::Connection,
}

impl ReisClient {
    pub async fn new() -> io::Result<Self> {
        let socket_path = std::env::var("LIBEI_SOCKET").unwrap_or_else(|_| "gamescope-0-ei".to_string());
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR not set"))?;
        let path = std::path::Path::new(&runtime_dir).join(socket_path);

        let stream = tokio::net::UnixStream::connect(path).await?;
        let stream = stream.into_std().map_err(io::Error::other)?;
        stream.set_nonblocking(true)?;
        let context = ei::Context::new(stream)?;
        let (connection, stream) = context.handshake_tokio("moonshine", ei::handshake::ContextType::Sender).await
            .map_err(io::Error::other)?;

        Ok(Self { stream, connection })
    }

    pub async fn next_event(&mut self) -> Option<Result<reis::event::EiEvent, reis::Error>> {
        self.stream.next().await
    }

    pub fn flush(&self) -> io::Result<()> {
        self.connection.flush().map_err(io::Error::other)
    }
}
