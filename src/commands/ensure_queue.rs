//! `catprinterd ensure-queue` — body of the root oneshot `catprinter-queue.service`: make sure the
//! CUPS queue exists, points at this daemon, and the PPD shows kid labels (Cat Tape, Text, Paper).
//! Prefers cups-filters `driverless` (applies printer-strings); falls back to `-m everywhere` plus
//! a label patch. Idempotent; self-heals at boot.

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

fn run_stdout(cmd: &str, args: &[&str]) -> (bool, String, String) {
    match Command::new(cmd).args(args).output() {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        ),
        Err(e) => (false, String::new(), format!("{cmd}: {e}")),
    }
}

/// GTK/LibreOffice read `*Keyword Choice/Human name:`, not printer-strings-uri.
fn ppd_has_kid_labels(ppd: &str) -> bool {
    ppd.contains("Cat Tape short")
        && (ppd.contains("Draft/Text") || ppd.contains("cupsPrintQuality Draft/Text"))
        && (ppd.contains("/Paper:") || ppd.contains("Stationery/Paper"))
        // Print Optimization duplicates Text/Photo/Graphics next to Print style.
        && !ppd.contains("*OpenUI *print-content-optimize")
}

fn same_ipp_target(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        s.trim()
            .trim_end_matches('/')
            .to_ascii_lowercase()
            .replace("localhost", "127.0.0.1")
    }
    let a = norm(a);
    let b = norm(b);
    !a.is_empty() && a == b
}

/// cups-filters `driverless URI` already substitutes our printer-strings into the PPD.
fn driverless_ppd(uri: &str) -> Option<String> {
    let (ok, out, err) = run_stdout("driverless", &[uri]);
    if ok && out.contains("*PPD-Adobe") && out.contains("*PageSize") {
        return Some(out);
    }
    tracing::debug!("driverless {uri}: ok={ok} stderr={}", err.trim());
    None
}

fn relabel_choice(line: &str, keyword: &str, choice: &str, label: &str) -> Option<String> {
    let star = format!("*{keyword} {choice}");
    let rest = line.strip_prefix(&star)?;
    let payload = if let Some(r) = rest.strip_prefix(':') {
        r
    } else if let Some(after_slash) = rest.strip_prefix('/') {
        after_slash.split_once(':')?.1
    } else {
        return None;
    };
    Some(format!("*{keyword} {choice}/{label}:{payload}"))
}

fn relabel_en_us(line: &str, keyword: &str, choice: &str, label: &str) -> Option<String> {
    let prefix = format!("*en_US.{keyword} {choice}/");
    let rest = line.strip_prefix(&prefix)?;
    let payload = rest.split_once(':')?.1;
    Some(format!("*en_US.{keyword} {choice}/{label}:{payload}"))
}

/// Drop a PPD `*OpenUI *NAME` … `*CloseUI: *NAME` block (and a following blank line).
fn strip_openui(ppd: &str, name: &str) -> String {
    let open = format!("*OpenUI *{name}");
    let close = format!("*CloseUI: *{name}");
    let mut out = String::with_capacity(ppd.len());
    let mut skipping = false;
    for line in ppd.lines() {
        if !skipping && line.starts_with(&open) {
            skipping = true;
            continue;
        }
        if skipping {
            if line.starts_with(&close) {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Left/right printable area is the full paper width (Gwenview/KDE otherwise defaults ~0.17 in).
fn force_zero_side_margins(ppd: &str) -> String {
    use std::collections::HashMap;
    let mut paper: HashMap<&str, &str> = HashMap::new();
    for line in ppd.lines() {
        let Some(rest) = line.strip_prefix("*PaperDimension ") else {
            continue;
        };
        let Some((name, vals)) = rest.split_once(':') else {
            continue;
        };
        let vals = vals.trim().trim_matches('"').trim();
        let Some((w, _)) = vals.split_once(char::is_whitespace) else {
            continue;
        };
        paper.insert(name.trim(), w.trim());
    }
    let mut out = String::with_capacity(ppd.len());
    for line in ppd.lines() {
        if let Some(rest) = line.strip_prefix("*ImageableArea ") {
            if let Some((name, vals)) = rest.split_once(':') {
                if let Some(w) = paper.get(name.trim()) {
                    let inner = vals.trim().trim_matches('"');
                    let mut p = inner.split_whitespace();
                    let _llx = p.next();
                    let lly = p.next().unwrap_or("0");
                    let _urx = p.next();
                    let ury = p.next().unwrap_or(lly);
                    out.push_str(&format!(
                        "*ImageableArea {}: \"0 {lly} {w} {ury}\"\n",
                        name.trim()
                    ));
                    continue;
                }
            }
        }
        if let Some(rest) = line.strip_prefix("*HWMargins:") {
            let inner = rest.trim().trim_matches('"');
            let mut p = inner.split_whitespace();
            let _l = p.next();
            let bottom = p.next().unwrap_or("0");
            let _r = p.next();
            let top = p.next().unwrap_or(bottom);
            out.push_str(&format!("*HWMargins: \"0 {bottom} 0 {top}\"\n"));
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Make CUPS `-m everywhere` PPDs show the same names driverless would.
fn apply_kid_labels(ppd: &str) -> String {
    let ppd = strip_openui(ppd, "print-content-optimize");
    let ppd = force_zero_side_margins(&ppd);
    let pairs: &[(&str, &str, &str)] = &[
        ("PageSize", "48x297mm", "Cat Tape short"),
        ("PageSize", "48x500mm", "Cat Tape long"),
        ("PageSize", "A4", "Cat Minidoc A4"),
        ("PageSize", "Letter", "Cat Minidoc Letter"),
        ("PageRegion", "48x297mm", "Cat Tape short"),
        ("PageRegion", "48x500mm", "Cat Tape long"),
        ("PageRegion", "A4", "Cat Minidoc A4"),
        ("PageRegion", "Letter", "Cat Minidoc Letter"),
        ("MediaType", "Stationery", "Paper"),
        ("MediaType", "Labels", "Sticker"),
        ("cupsPrintQuality", "Draft", "Text"),
        ("cupsPrintQuality", "Normal", "Default"),
        ("cupsPrintQuality", "High", "Picture"),
        ("ColorModel", "Gray", "Grayscale"),
        ("ColorModel", "FastGray", "Black and white"),
    ];
    let mut out = String::with_capacity(ppd.len() + 256);
    for line in ppd.lines() {
        let mut done = false;
        for &(kw, choice, label) in pairs {
            if let Some(s) = relabel_choice(line, kw, choice, label) {
                out.push_str(&s);
                done = true;
                break;
            }
            if let Some(s) = relabel_en_us(line, kw, choice, label) {
                out.push_str(&s);
                done = true;
                break;
            }
        }
        if !done {
            if line == "*OpenUI *cupsPrintQuality: PickOne"
                || line.starts_with("*OpenUI *cupsPrintQuality/")
            {
                out.push_str("*OpenUI *cupsPrintQuality/Print style: PickOne");
            } else if line == "*OpenUI *MediaType: PickOne"
                || line.starts_with("*OpenUI *MediaType/")
            {
                out.push_str("*OpenUI *MediaType/Paper type: PickOne");
            } else if line.starts_with("*en_US.Translation cupsPrintQuality/") {
                out.push_str("*en_US.Translation cupsPrintQuality/Print style: \"\"");
            } else {
                out.push_str(line);
            }
        }
        out.push('\n');
    }
    if !out.contains("*ColorModel FastGray") && out.contains("*OpenUI *ColorModel") {
        out = out.replace(
            "*OpenUI *ColorModel: PickOne\n",
            "*OpenUI *ColorModel/Tone: PickOne\n*ColorModel FastGray/Black and white: \"\"\n*en_US.ColorModel FastGray/Black and white: \"\"\n",
        );
        if !out.contains("*OpenUI *ColorModel/Tone") {
            out = out.replace(
                "*OpenUI *ColorModel/Color Mode: PickOne\n",
                "*OpenUI *ColorModel/Tone: PickOne\n*ColorModel FastGray/Black and white: \"\"\n*en_US.ColorModel FastGray/Black and white: \"\"\n",
            );
        }
    }
    out
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
    let installed_ppd = std::fs::read_to_string(&ppd_path).unwrap_or_default();
    let ppd_ok = ppd_has_kid_labels(&installed_ppd);
    let stamp_path = "/var/lib/catprinter/queue.stamp";
    let want_stamp = format!("{}|{}|{}|labels", crate::VERSION, uri, a.location);
    let stamp_ok = std::fs::read_to_string(stamp_path)
        .map(|c| c.trim() == want_stamp)
        .unwrap_or(false);
    // Skip regenerating the PPD when nothing changed: re-running lpadmin every boot rewrites
    // the PPD and resets any per-printer `lpadmin -o` defaults an admin set.
    if current_dev
        .as_deref()
        .is_some_and(|d| same_ipp_target(d, &uri))
        && ppd_ok
        && stamp_ok
    {
        let (_, _) = run("cupsenable", &[&queue]);
        let (_, _) = run("cupsaccept", &[&queue]);
        remove_duplicate_queues(&queue, &uri);
        println!("✔ queue {queue} already correct (kid-labelled PPD)");
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
    // cups-filters `driverless` bakes printer-strings into /Human names. CUPS `-m everywhere`
    // does not (GTK then shows Draft / Stationery / 48x297mm).
    let mut ppd_src = driverless_ppd(&uri);
    if ppd_src.is_some() {
        println!("• using cups-filters driverless PPD (kid labels)");
    } else {
        println!("• driverless not usable; falling back to -m everywhere + label patch");
        if !lpadmin_install(&queue, &uri, &a.location, &["-m", "everywhere"]).await {
            return 1;
        }
        let raw = std::fs::read_to_string(&ppd_path).unwrap_or_default();
        if raw.is_empty() {
            eprintln!("✘ could not read {ppd_path} after lpadmin");
            return 1;
        }
        ppd_src = Some(apply_kid_labels(&raw));
    }
    let labelled = apply_kid_labels(&ppd_src.unwrap());
    let tmp = std::env::temp_dir().join(format!("catprinterd-{queue}.ppd"));
    if let Err(e) = std::fs::write(&tmp, &labelled) {
        eprintln!("✘ write {}: {e}", tmp.display());
        return 1;
    }
    let tmp_s = tmp.to_string_lossy().into_owned();
    if !lpadmin_install(&queue, &uri, &a.location, &["-P", &tmp_s]).await {
        let _ = std::fs::remove_file(&tmp);
        return 1;
    }
    let _ = std::fs::remove_file(&tmp);
    let ppd = std::fs::read_to_string(&ppd_path).unwrap_or_default();
    if ppd_has_kid_labels(&ppd) {
        println!("✔ PPD labels: Cat Tape / Cat Minidoc, Text/Default/Picture, Paper/Sticker");
    } else if ppd.contains("*PageSize 48x297mm") {
        eprintln!("! PPD has tape size but not kid labels — check `lpoptions -p {queue} -l`");
    } else if ppd.is_empty() {
        println!("• could not re-read {ppd_path} (not root?) — skipping PPD check");
    } else {
        eprintln!("! generated PPD lacks '*PageSize 48x297mm'; check `lpoptions -p {queue} -l`");
    }
    remove_duplicate_queues(&queue, &uri);
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

async fn lpadmin_install(queue: &str, uri: &str, location: &str, model: &[&str]) -> bool {
    let mut args = vec![
        "-p",
        queue,
        "-E",
        "-v",
        uri,
        "-D",
        "Cat Printer",
        "-L",
        location,
        "-o",
        "printer-error-policy=retry-job",
        "-o",
        "printer-is-shared=false",
        "-u",
        "allow:all",
    ];
    args.extend_from_slice(model);
    let mut last = String::new();
    for attempt in 1..=3 {
        let (ok, out) = run("lpadmin", &args);
        last = out;
        if ok {
            return true;
        }
        eprintln!("lpadmin attempt {attempt}/3 failed: {}", last.trim());
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    eprintln!("✘ lpadmin failed: {}", last.trim());
    false
}

/// GNOME/driverless often adds a second queue (`Cat_Printer`) for the same loopback URI.
fn remove_duplicate_queues(keep: &str, uri: &str) {
    let (_, v) = run("lpstat", &["-v"]);
    for line in v.lines() {
        let rest = line.strip_prefix("device for ").unwrap_or("");
        let Some((name, dev)) = rest.split_once(": ") else {
            continue;
        };
        if name != keep && same_ipp_target(dev.trim(), uri) {
            let (ok, out) = run("lpadmin", &["-x", name]);
            if ok {
                println!("✔ removed duplicate queue {name} (same URI as {keep})");
            } else {
                eprintln!("! could not remove duplicate {name}: {}", out.trim());
            }
        }
    }
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
    use super::{apply_kid_labels, ppd_has_kid_labels, same_ipp_target, valid_queue_name};

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

    #[test]
    fn localhost_and_loopback_are_the_same_target() {
        assert!(same_ipp_target(
            "ipp://localhost:8095/ipp/print",
            "ipp://127.0.0.1:8095/ipp/print"
        ));
        assert!(!same_ipp_target(
            "ipp://127.0.0.1:8095/ipp/print",
            "ipp://127.0.0.1:631/ipp/print"
        ));
    }

    #[test]
    fn everywhere_ppd_gets_kid_labels() {
        let raw = r#"
*OpenUI *PageSize: PickOne
*PageSize 48x297mm: "<</PageSize[136 842]>>setpagedevice"
*PageSize 48x500mm: "<</PageSize[136 1417]>>setpagedevice"
*PageSize A4: "<</PageSize[595 842]>>setpagedevice"
*PageSize Letter: "<</PageSize[612 792]>>setpagedevice"
*CloseUI: *PageSize
*OpenUI *MediaType: PickOne
*MediaType Stationery: "<</MediaType(Stationery)>>setpagedevice"
*en_US.MediaType Stationery/Stationery: ""
*MediaType Labels: "<</MediaType(Labels)>>setpagedevice"
*en_US.MediaType Labels/Labels: ""
*CloseUI: *MediaType
*OpenUI *cupsPrintQuality: PickOne
*en_US.Translation cupsPrintQuality/Print Quality: ""
*cupsPrintQuality Draft: "<</HWResolution[203 203]>>setpagedevice"
*en_US.cupsPrintQuality Draft/Draft: ""
*cupsPrintQuality Normal: "<</HWResolution[203 203]>>setpagedevice"
*en_US.cupsPrintQuality Normal/Normal: ""
*cupsPrintQuality High: "<</HWResolution[203 203]>>setpagedevice"
*en_US.cupsPrintQuality High/High: ""
*CloseUI: *cupsPrintQuality
*OpenUI *print-content-optimize/Print Optimization: PickOne
*print-content-optimize auto/Automatic: ""
*print-content-optimize photo/Photo: ""
*print-content-optimize text/Text: ""
*print-content-optimize graphic/Graphics: ""
*CloseUI: *print-content-optimize
*OpenUI *ColorModel: PickOne
*ColorModel Gray: "<</cupsColorSpace 18>>setpagedevice"
*en_US.ColorModel Gray/Grayscale: ""
*DefaultColorModel: FastGray
*CloseUI: *ColorModel
*PaperDimension 48x297mm: "136.06 841.89"
*ImageableArea 48x297mm: "12 2.83 124 839"
*HWMargins: "12 2.83 12 2.83"
"#;
        assert!(!ppd_has_kid_labels(raw));
        let labelled = apply_kid_labels(raw);
        assert!(ppd_has_kid_labels(&labelled), "{labelled}");
        assert!(labelled.contains("*PageSize 48x297mm/Cat Tape short:"));
        assert!(labelled.contains("*PageSize A4/Cat Minidoc A4:"));
        assert!(labelled.contains("*cupsPrintQuality Draft/Text:"));
        assert!(labelled.contains("*en_US.MediaType Stationery/Paper:"));
        assert!(labelled.contains("*ColorModel FastGray/Black and white:"));
        assert!(labelled.contains("*OpenUI *cupsPrintQuality/Print style:"));
        assert!(
            !labelled.contains("print-content-optimize"),
            "Print Optimization duplicates Print style"
        );
        assert!(labelled.contains("*ImageableArea 48x297mm: \"0 2.83 136.06 839\""));
        assert!(labelled.contains("*HWMargins: \"0 2.83 0 2.83\""));
        // idempotent
        assert_eq!(apply_kid_labels(&labelled), labelled);
    }
}
