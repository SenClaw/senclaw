use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};

/// A simple wrapper around TcpListener to simulate Node.js `net.createServer`.
pub struct Server {
    listener: Option<TcpListener>,
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    pub fn new() -> Self {
        Self { listener: None }
    }

    /// Bind the server to a host and port.
    pub async fn listen(&mut self, addr: &str) -> Result<()> {
        let listener = TcpListener::bind(addr).await?;
        self.listener = Some(listener);
        Ok(())
    }

    /// Accept a single connection.
    pub async fn accept(&self) -> Result<Option<TcpStream>> {
        if let Some(ref listener) = self.listener {
            let (socket, _) = listener.accept().await?;
            Ok(Some(socket))
        } else {
            Ok(None)
        }
    }
}
