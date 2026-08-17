//! End-to-end: spawn `catprinterd serve --fake-printer`, drive it with CUPS's `ipptool` suites
//! (skipped when ipptool is not installed), and check the fake printer produced output.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    port: u16,
    dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn_daemon(extra: &[&str]) -> Daemon {
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catprinterd"));
    cmd.args(["serve", "--port", &port.to_string(), "--fake-printer"])
        .arg(dir.path())
        .args([
            "--dnssd",
            "off",
            "--uuid",
            "11111111-2222-3333-4444-555555555555",
        ])
        .args(if extra.contains(&"--printer-wait") {
            &[][..]
        } else {
            &["--printer-wait", "5"][..]
        })
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let child = cmd.spawn().expect("spawn catprinterd");
    let d = Daemon { child, port, dir };
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            std::thread::sleep(Duration::from_millis(200));
            return d;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("daemon did not start");
}

/// When CATPRINTER_REQUIRE_IPPTOOL=1 (set in CI), the ipptool suites must run — a missing
/// ipptool becomes a hard failure instead of a silent skip, so CI can't go green testing nothing.
fn require_or_skip() -> bool {
    if ipptool_available() {
        return true;
    }
    if std::env::var("CATPRINTER_REQUIRE_IPPTOOL").as_deref() == Ok("1") {
        panic!("ipptool / /usr/share/cups/ipptool/ipp-everywhere.test not found but CATPRINTER_REQUIRE_IPPTOOL=1");
    }
    eprintln!("ipptool not available — skipping");
    false
}

fn ipptool_available() -> bool {
    Command::new("ipptool")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success() || s.code() == Some(1))
        .unwrap_or(false)
        && PathBuf::from("/usr/share/cups/ipptool/ipp-everywhere.test").exists()
}

fn run_ipptool(port: u16, file: Option<&str>, test: &str, extra: &[&str]) -> (bool, String) {
    let mut cmd = Command::new("ipptool");
    cmd.args(["-tv"]);
    if let Some(f) = file {
        cmd.args(["-f", f, "-d", "filetype=image/pwg-raster"]);
    }
    cmd.args(extra);
    cmd.arg(format!("ipp://127.0.0.1:{port}/ipp/print"));
    cmd.arg(if test.starts_with('/') {
        test.to_string()
    } else {
        format!("/usr/share/cups/ipptool/{test}")
    });
    let out = cmd.output().expect("run ipptool");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn health_and_status_page() {
    let d = spawn_daemon(&[]);
    let body = ureq_get(d.port, "/health");
    assert!(body.contains("\"version\""), "{body}");
    assert!(body.contains("\"printer_state\": \"idle\""), "{body}");
    let page = ureq_get(d.port, "/");
    assert!(page.contains("Cat Printer"));
    let strings = ureq_get(d.port, "/strings/en.strings");
    assert!(strings.contains("Cat tape 48 mm"));
}

/// Minimal HTTP GET (no client crate needed).
fn ureq_get(port: u16, path: &str) -> String {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn ipp_everywhere_suite_passes() {
    if !require_or_skip() {
        return;
    }
    let d = spawn_daemon(&[]);
    let (_ok, text) = run_ipptool(
        d.port,
        Some(&fixture("text-roll48.pwg")),
        "ipp-everywhere.test",
        &["-V", "2.0", "-I"],
    );
    let fails: Vec<&str> = text.lines().filter(|l| l.contains("[FAIL]")).collect();
    let allow = std::fs::read_to_string(format!(
        "{}/tests/ipptool/allowlist.txt",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_default();
    let unexpected: Vec<&&str> = fails
        .iter()
        .filter(|f| {
            !allow
                .lines()
                .any(|a| !a.trim().is_empty() && f.contains(a.trim()))
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "ipp-everywhere.test failures:\n{}\n--- full log ---\n{text}",
        unexpected
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        text.lines().filter(|l| l.contains("[PASS]")).count() >= 25,
        "suspiciously few passes:\n{text}"
    );
    // the suite printed the fixture (Print-Job tests) → fake output exists
    let pngs = std::fs::read_dir(d.dir.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".png")
        })
        .count();
    assert!(pngs >= 1, "fake printer produced no PNG");
}

#[test]
fn cups_suites_pass() {
    if !require_or_skip() {
        return;
    }
    let d = spawn_daemon(&[]);
    for t in [
        "get-printer-attributes.test",
        "get-jobs.test",
        "print-job-and-wait.test",
        "ipp-backend.test",
        "identify-printer.test",
        "print-job-media-col.test",
        "create-job.test",
        "validate-job.test",
        "get-completed-jobs.test",
    ] {
        let (ok, text) = run_ipptool(d.port, Some(&fixture("photo-roll48.pwg")), t, &[]);
        assert!(ok && !text.contains("[FAIL]"), "{t} failed:\n{text}");
    }
    // Job ids are monotonic (time-based) so the first job is not id 1 — find the job JSON.
    let job_json = std::fs::read_dir(d.dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            let n = p.file_name().unwrap().to_string_lossy();
            n.starts_with("job-") && n.ends_with(".json")
        })
        .expect("fake printer wrote no job-*.json");
    let json = std::fs::read_to_string(&job_json).unwrap();
    assert!(json.contains("\"width\": 384"), "{json}");
    // Identify-Printer is fire-and-forget on the worker (IPP returns before the
    // fake printer writes). Later suites in this list enqueue more print jobs, so
    // the marker may appear a beat after ipptool exits — poll, and glob any
    // identify-*.txt (same class of flake as the old job-1.json assert).
    let deadline = Instant::now() + Duration::from_secs(3);
    let ident = loop {
        let found = std::fs::read_dir(d.dir.path())
            .unwrap()
            .flatten()
            .find(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                n.starts_with("identify-") && n.ends_with(".txt")
            });
        if found.is_some() || Instant::now() >= deadline {
            break found;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        ident.is_some(),
        "fake printer wrote no identify-*.txt; dir={:?}",
        std::fs::read_dir(d.dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect::<Vec<_>>()
    );
}

#[test]
fn printer_off_then_on_and_give_up() {
    if !require_or_skip() {
        return;
    }
    let d = spawn_daemon(&["--printer-wait", "4"]);
    std::fs::write(d.dir.path().join("state"), "off").unwrap();
    // print-job-and-wait polls until the job completes/aborts; expect it to end aborted (>4 s)
    let (_ok, text) = run_ipptool(
        d.port,
        Some(&fixture("text-roll48.pwg")),
        "print-job-and-wait.test",
        &[],
    );
    assert!(
        text.contains("job-state (enum) = aborted") || text.contains("job-state (enum) = 8"),
        "expected aborted job:\n{text}"
    );
    let health = ureq_get(d.port, "/health");
    assert!(health.contains("offline-report"), "{health}");
    // recovery: printer on, next job prints
    std::fs::write(d.dir.path().join("state"), "ok").unwrap();
    let (ok, text) = run_ipptool(
        d.port,
        Some(&fixture("text-roll48.pwg")),
        "print-job-and-wait.test",
        &[],
    );
    assert!(ok, "{text}");
    assert!(text.contains("completed") || text.contains("= 9"), "{text}");
    let health = ureq_get(d.port, "/health");
    assert!(
        !health.contains("offline-report"),
        "sticky error should clear on success: {health}"
    );
}

#[test]
fn oversize_document_is_rejected_cleanly() {
    let d = spawn_daemon(&["--max-document-mb", "1"]);
    // Build a Print-Job with a 2 MiB body by hand.
    use std::io::{Read, Write};
    let mut req = vec![2u8, 0, 0, 2, 0, 0, 0, 9, 1];
    let attr = |b: &mut Vec<u8>, tag: u8, name: &str, val: &str| {
        b.push(tag);
        b.extend((name.len() as u16).to_be_bytes());
        b.extend(name.as_bytes());
        b.extend((val.len() as u16).to_be_bytes());
        b.extend(val.as_bytes());
    };
    attr(&mut req, 0x47, "attributes-charset", "utf-8");
    attr(&mut req, 0x48, "attributes-natural-language", "en");
    attr(
        &mut req,
        0x45,
        "printer-uri",
        &format!("ipp://127.0.0.1:{}/ipp/print", d.port),
    );
    req.push(3);
    req.extend(b"RaS2");
    req.resize(2 * 1024 * 1024, 0);
    let mut s = std::net::TcpStream::connect(("127.0.0.1", d.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(s, "POST /ipp/print HTTP/1.1\r\nHost: x\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\n\r\n", req.len()).unwrap();
    let _ = s.write_all(&req); // may fail with EPIPE once the server answers early — fine
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    // IPP status 0x0409 client-error-request-value-too-long
    let body_start = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let body = &out[body_start..];
    assert_eq!(&body[2..4], &[0x04, 0x09], "status bytes {:?}", &body[..8]);
}
