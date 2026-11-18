//! Generated types for `sandfork.v1`, plus the client connector.
//!
//! The proto file in `proto/sandfork.proto` is the contract; nothing here is
//! written by hand except [`connect`].

pub mod v1 {
    tonic::include_proto!("sandfork.v1");
}

pub use v1::sandfork_client::SandforkClient;
pub use v1::sandfork_server::{Sandfork, SandforkServer};

use tonic::transport::{Channel, Endpoint, Uri};

/// Largest message either side accepts. Larger files move in chunks.
pub const MAX_MESSAGE: usize = 16 << 20;

/// Connect to a daemon. `addr` is `unix:///path/to/sandfork.sock` (or a bare
/// path), or an `http://` / `https://` URL for a remote daemon.
pub async fn connect(addr: &str) -> Result<SandforkClient<Channel>, tonic::transport::Error> {
    let channel = if addr.starts_with("http://") || addr.starts_with("https://") {
        Endpoint::try_from(addr.to_string())?.connect().await?
    } else {
        let path = addr.strip_prefix("unix://").unwrap_or(addr).to_string();
        // The URI is ignored: the connector always dials the socket path.
        Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let path = path.clone();
                async move {
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                        tokio::net::UnixStream::connect(path).await?,
                    ))
                }
            }))
            .await?
    };
    Ok(SandforkClient::new(channel)
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE))
}

/// The default socket path: `$XDG_RUNTIME_DIR/sandfork.sock`, falling back to
/// `/tmp/sandfork-<uid>.sock`.
pub fn default_socket() -> String {
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(d) if !d.is_empty() => format!("{d}/sandfork.sock"),
        _ => format!(
            "/tmp/sandfork-{}.sock",
            std::env::var("UID").unwrap_or_else(|_| "user".into())
        ),
    }
}
