//! `catprinterd ensure-queue` — body of the root oneshot `catprinter-queue.service`: make sure the
//! CUPS queue exists, points at this daemon and uses `-m everywhere`; idempotent; self-heals at boot.

use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::EnsureQueueArgs;

fn run(cmd: &str, args: &[&str]) -> (bool, String) {
    match Command::new(cmd).args(args).output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s)
        }
        Err(e) => (false, format!("{cmd}: {e}")),
    }
}

async fn health_ok(port: u16) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(Ok(mut s)) = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    else {
        return false;
    };
    if tokio::time::timeout(
        Duration::from_secs(2),
        s.write_all(
            format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        ),
    )
    .await
    .map(|r| r.is_err())
    .unwrap_or(true)
    {
        return false;
    }
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf);
    text.starts_with("HTTP/1.1 200") && text.contains("\"version\"")
}

/// CUPS printer names: printable ASCII, no space/tab and none of / # ?, 1..=127 chars, and not
/// starting with `-` (which would be read as an lpadmin option). Reject rather than shell-quote.
fn valid_queue_name(q: &str) -> bool {
    !q.is_empty()
        && q.len() <= 127
        && !q.starts_with('-')
        && q.bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'/' | b'#' | b'?'))
}

pub async fn ensure_queue(a: EnsureQueueArgs) -> i32 {
    let queue = a.queue.clone();
    if !valid_queue_name(&queue) {
        eprintln!("✘ invalid queue name {queue:?} (printable ASCII, no / # ? or spaces, ≤127, no leading '-')");
        return 1;
    }
    if a.location.starts_with('-') {
        eprintln!("✘ location must not start with '-'");
        return 1;
    }
    let uri = format!("ipp://127.0.0.1:{}/ipp/print", a.port);
    if a.remove {
        let (ok, out) = run("lpadmin", &["-x", &queue]);
        if ok || out.contains("does not exist") || out.contains("not found") {
            println!("✔ queue {queue} removed");
            return 0;
        }
        eprintln!("✘ lpadmin -x {queue}: {out}");
        return 1;
    }
    let deadline = Instant::now() + Duration::from_secs(a.wait.max(1));
    // wait for cupsd
    loop {
        let (ok, out) = run("lpstat", &["-r"]);
        if ok && out.contains("is running") {
            break;
        }
        if Instant::now() > deadline {
            eprintln!("✘ cupsd is not running (lpstat -r: {})", out.trim());
            return 1;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    // wait for the daemon
    loop {
        if health_ok(a.port).await {
            break;
        }
        if Instant::now() > deadline {
            eprintln!(
                "✘ catprinterd is not answering on 127.0.0.1:{} — is catprinter.service running?",
                a.port
            );
            return 1;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    // existing queue?
    let (_, v) = run("lpstat", &["-v", &queue]);
    let current_dev = v
        .lines()
        .filter(|l| l.starts_with("device for "))
        .find_map(|l| l.split_once(": ").map(|(_, d)| d.trim().to_string()));
    let ppd_path = format!("/etc/cups/ppd/{queue}.ppd");
    let ppd_ok = std::fs::read_to_string(&ppd_path)
        .map(|p| p.contains("*PageSize 48x297mm"))
        .unwrap_or(false);
    let stamp_path = "/var/lib/catprinter/queue.stamp";
    let want_stamp = format!("{}|{}|{}", crate::VERSION, uri, a.location);
    let stamp_ok = std::fs::read_to_string(stamp_path)
        .map(|c| c.trim() == want_stamp)
        .unwrap_or(false);
    // Skip regenerating the PPD when nothing changed: re-running `lpadmin -m everywhere` every
    // boot rewrites the PPD and resets any per-printer `lpadmin -o` defaults an admin set.
    if current_dev.as_deref() == Some(uri.as_str()) && ppd_ok && stamp_ok {
        let (_, _) = run("cupsenable", &[&queue]);
        let (_, _) = run("cupsaccept", &[&queue]);
        println!("✔ queue {queue} already correct (skipping lpadmin -m everywhere)");
        poke_refresh(a.port).await;
        return 0;
    }
    match &current_dev {
        Some(dev) if dev != &uri => {
            println!("• queue {queue} pointed at {dev}; repointing to {uri}")
        }
        Some(_) => println!("• queue {queue} exists; refreshing (PPD or version changed)"),
        None => println!("• queue {queue} does not exist yet; creating"),
    }
    // lpadmin -m everywhere (cupsd fetches our attributes and generates the PPD)
    let mut last = String::new();
    let mut created = false;
    for attempt in 1..=3 {
        let (ok, out) = run(
            "lpadmin",
            &[
                "-p",
                &queue,
                "-E",
                "-v",
                &uri,
                "-m",
                "everywhere",
                "-D",
                "Cat Printer",
                "-L",
                &a.location,
                "-o",
                "printer-error-policy=retry-job",
                "-o",
                "printer-is-shared=false",
                "-u",
                "allow:all",
            ],
        );
        last = out.clone();
        if ok {
            created = true;
            break;
        }
        eprintln!("lpadmin attempt {attempt}/3 failed: {}", out.trim());
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if !created {
        eprintln!("✘ lpadmin -m everywhere failed: {}", last.trim());
        return 1;
    }
    for warn in last.lines().filter(|l| l.contains("deprecated")) {
        tracing::debug!("{warn}");
    }
    // sanity: generated PPD has our tape size
    let ppd = std::fs::read_to_string(format!("/etc/cups/ppd/{queue}.ppd")).unwrap_or_default();
    if ppd.contains("*PageSize 48x297mm") {
        println!("✔ generated PPD has the 48x297mm tape size");
    } else if ppd.is_empty() {
        println!("• could not read /etc/cups/ppd/{queue}.ppd (not root?) — skipping PPD check");
    } else {
        eprintln!("! generated PPD lacks '*PageSize 48x297mm' — CUPS named the sizes differently; check `lpoptions -p {queue} -l`");
    }
    let (_, _) = run("cupsenable", &[&queue]);
    let (_, _) = run("cupsaccept", &[&queue]);
    let (_, p) = run("lpstat", &["-p", &queue]);
    println!("✔ {}", p.lines().next().unwrap_or("queue ready").trim());
    let (_, d) = run("lpstat", &["-d"]);
    if d.contains(&format!("destination: {queue}")) {
        eprintln!("!! {queue} is the SYSTEM DEFAULT printer — homework would land on 48 mm tape. Fix: lpadmin -d <other-printer>");
    }
    // Record what we just made so the next boot can skip the PPD rebuild.
    let _ = std::fs::create_dir_all("/var/lib/catprinter");
    if let Err(e) = std::fs::write(stamp_path, format!("{want_stamp}\n")) {
        tracing::debug!("could not write {stamp_path}: {e}");
    }
    poke_refresh(a.port).await;
    println!("✔ queue {queue} → {uri}");
    0
}

/// Nudge the running daemon to adopt this queue's uuid now (GET /health?refresh); best-effort.
async fn poke_refresh(port: u16) {
    use tokio::io::AsyncWriteExt;
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        s.write_all(
            format!("GET /health?refresh HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .ok()?;
        Some(())
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::valid_queue_name;

    #[test]
    fn queue_name_validation() {
        assert!(valid_queue_name("CatPrinter"));
        assert!(valid_queue_name("cat_printer-2"));
        assert!(!valid_queue_name(""));
        assert!(!valid_queue_name("-x")); // option injection
        assert!(!valid_queue_name("has space"));
        assert!(!valid_queue_name("a/b"));
        assert!(!valid_queue_name("a#b"));
        assert!(!valid_queue_name(&"q".repeat(200)));
    }
}
