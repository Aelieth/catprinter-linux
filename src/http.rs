//! HTTP/1.1 front end (hyper): IPP over POST, /health, status page. Filled in M2; stub serves /health.

use anyhow::Result;

use crate::config::ServeArgs;

pub async fn serve(args: ServeArgs) -> Result<()> {
    let addr = std::net::SocketAddr::new(args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        "catprinterd {} listening on http://{addr}/ (stub)",
        crate::VERSION
    );
    loop {
        let (mut sock, _) = listener.accept().await?;
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let body = format!("{{\"version\":\"{}\",\"stub\":true}}\n", crate::VERSION);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}
