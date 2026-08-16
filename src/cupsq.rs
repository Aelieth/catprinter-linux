//! Talk to the local cupsd (127.0.0.1:631) with our own IPP codec + a minimal HTTP/1.1 client, to
//! adopt the CUPS queue's `printer-uuid`: libcups hides the DNS-SD-discovered "Cat Printer" next to
//! the `CatPrinter` queue in print dialogs only when the TXT `UUID` equals the queue's uuid.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::ipp::codec::{parse, v_kws, v_name, Group, ReqBuilder};

/// One IPP round trip to cupsd. Returns the parsed response.
pub async fn cups_request(path: &str, body: Bytes) -> anyhow::Result<crate::ipp::codec::Request> {
    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(("127.0.0.1", 631)),
    )
    .await??;
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost:631\r\nUser-Agent: catprinterd/{}\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        crate::VERSION,
        body.len()
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&body).await
    })
    .await
    .map_err(|_| anyhow::anyhow!("cupsd write timed out"))??;
    let mut raw = Vec::with_capacity(8192);
    tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut raw)).await??;
    let (status, hdrs, body) = split_http(&raw)?;
    if status != 200 {
        anyhow::bail!("cupsd HTTP {status}");
    }
    let body = if hdrs
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    Ok(parse(Bytes::from(body))?)
}

fn split_http(raw: &[u8]) -> anyhow::Result<(u16, String, &[u8])> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("no HTTP header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..sep]).to_string();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("bad status line"))?;
    Ok((status, head, &raw[sep + 4..]))
}

fn dechunk(mut b: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let nl = b
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| anyhow::anyhow!("bad chunk"))?;
        let size_str = std::str::from_utf8(&b[..nl])?
            .split(';')
            .next()
            .unwrap_or("0")
            .trim();
        let size = usize::from_str_radix(size_str, 16)?;
        b = &b[nl + 2..];
        if size == 0 {
            break;
        }
        if b.len() < size {
            anyhow::bail!("short chunk");
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size..];
        if b.starts_with(b"\r\n") {
            b = &b[2..];
        }
    }
    Ok(out)
}

/// Ask cupsd for the queue's printer-uuid: first by name, else any queue whose device-uri points at us.
pub async fn queue_uuid(queue: &str, our_port: u16) -> anyhow::Result<Option<(String, String)>> {
    // 1) by name
    let uri = format!("ipp://localhost/printers/{queue}");
    let req = ReqBuilder::new(ipp::model::Operation::GetPrinterAttributes, &uri)?
        .add(
            Group::OperationAttributes,
            "requesting-user-name",
            v_name("catprinterd"),
        )
        .add(
            Group::OperationAttributes,
            "requested-attributes",
            v_kws(&["printer-uuid", "device-uri", "printer-name"]),
        )
        .into_bytes();
    if let Ok(resp) = cups_request(&format!("/printers/{queue}"), req).await {
        if resp.op == 0 {
            if let Some(u) = resp.get_str(Some(Group::PrinterAttributes), "printer-uuid") {
                return Ok(Some((queue.to_string(), u)));
            }
        }
    }
    // 2) any queue pointing at our port
    let req = ReqBuilder::new(ipp::model::Operation::CupsGetPrinters, "ipp://localhost/")?
        .add(
            Group::OperationAttributes,
            "requesting-user-name",
            v_name("catprinterd"),
        )
        .add(
            Group::OperationAttributes,
            "requested-attributes",
            v_kws(&["printer-uuid", "device-uri", "printer-name"]),
        )
        .into_bytes();
    let resp = cups_request("/", req).await?;
    let needle = format!(":{our_port}/ipp/print");
    for g in resp
        .attrs
        .groups()
        .iter()
        .filter(|g| g.tag() == Group::PrinterAttributes)
    {
        let get = |n: &str| {
            g.get(n)
                .and_then(|a| crate::ipp::codec::value_to_string(a.value()))
        };
        if let (Some(dev), Some(uuid), Some(name)) =
            (get("device-uri"), get("printer-uuid"), get("printer-name"))
        {
            if dev.contains(&needle)
                && (dev.contains("127.0.0.1") || dev.contains("localhost") || dev.contains("[::1]"))
            {
                return Ok(Some((name, uuid)));
            }
        }
    }
    Ok(None)
}

/// Background task: adopt the queue's uuid, re-checking periodically (queues get created at boot).
pub async fn adopt_uuid_task(
    queue: String,
    uuid: Arc<RwLock<String>>,
    tx: tokio::sync::watch::Sender<String>,
    adopt_now: Arc<tokio::sync::Notify>,
    shutdown: CancellationToken,
) {
    let mut delays = [5u64, 15, 30, 60, 120, 300].into_iter();
    let mut logged = false;
    loop {
        let wait = delays.next().unwrap_or(600);
        // Slow backoff normally, but `/health?refresh` (ensure-queue after creating the queue)
        // wakes us at once so the DNS-SD UUID lines up within a second or two at first boot.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(wait)) => {},
            _ = adopt_now.notified() => { delays = [5u64, 15, 30, 60, 120, 300].into_iter(); }
            _ = shutdown.cancelled() => return,
        }
        let our_port = OUR_PORT.load(std::sync::atomic::Ordering::Relaxed);
        match queue_uuid(&queue, our_port).await {
            Ok(Some((name, u))) => {
                let u = if u.starts_with("urn:uuid:") {
                    u
                } else {
                    format!("urn:uuid:{u}")
                };
                let changed = uuid.read().map(|cur| *cur != u).unwrap_or(false);
                if changed {
                    if let Ok(mut w) = uuid.write() {
                        *w = u.clone();
                    }
                    let _ = tx.send(u.clone());
                    tracing::info!("adopted printer-uuid {u} from CUPS queue '{name}'");
                }
                logged = false;
            }
            Ok(None) => {
                if !logged {
                    tracing::info!("cups: no queue for us yet (looking for '{queue}' or a queue pointing at :{our_port}); will keep checking");
                    logged = true;
                }
            }
            Err(e) => {
                if !logged {
                    tracing::info!("cups: not reachable ({e}); will keep checking");
                    logged = true;
                }
            }
        }
    }
}

/// Our own port, set by `serve` so the adopter can match device-uris.
pub static OUR_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(8095);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dechunk_works() {
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(dechunk(raw).unwrap(), b"Wikipedia");
        let (st, _h, body) = split_http(b"HTTP/1.1 200 OK\r\nX: y\r\n\r\nbody").unwrap();
        assert_eq!(st, 200);
        assert_eq!(body, b"body");
    }
}
