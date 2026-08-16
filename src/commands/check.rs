//! `catprinterd check`: is the system ready to print? D-Bus, bluetoothd, a powered adapter, and
//! whether our port is free or already answered by a running daemon. Exit 0 only when ready.

use std::time::Duration;

use crate::ble::{bluez, discovery};
use crate::config::CheckArgs;

pub async fn run(args: CheckArgs) -> i32 {
    let mut ready = true;
    println!("catprinterd {}", crate::VERSION);

    // System bus + bluetoothd.
    let conn = match tokio::time::timeout(Duration::from_secs(3), zbus::Connection::system()).await
    {
        Ok(Ok(c)) => {
            println!("{:<16} ok", "system bus");
            Some(c)
        }
        _ => {
            println!("{:<16} MISSING — no system D-Bus", "system bus");
            ready = false;
            None
        }
    };

    if let Some(conn) = &conn {
        // `catprinterd check` runs inline in install.sh — a wedged dbus-daemon must not hang it.
        let probe = async {
            let dbus = zbus::fdo::DBusProxy::new(conn).await.ok()?;
            dbus.name_has_owner("org.bluez".try_into().ok()?).await.ok()
        };
        let owned = tokio::time::timeout(std::time::Duration::from_secs(3), probe)
            .await
            .ok()
            .flatten()
            .unwrap_or(false);
        if owned {
            println!("{:<16} ok", "bluetoothd");
        } else {
            println!(
                "{:<16} MISSING — org.bluez not running (start bluetooth.service)",
                "bluetoothd"
            );
            ready = false;
        }

        // Adapters.
        match bluez::managed_objects(conn).await {
            Ok(objs) => {
                let adapters = discovery::adapters_from(&objs);
                if adapters.is_empty() {
                    println!("{:<16} MISSING — no Bluetooth adapter", "adapter");
                    ready = false;
                } else {
                    let powered = adapters.iter().find(|a| a.powered);
                    match powered {
                        Some(a) => println!(
                            "{:<16} {} powered ({})",
                            "adapter",
                            a.path.rsplit('/').next().unwrap_or(&a.path),
                            a.address
                        ),
                        None => {
                            println!("{:<16} NOT POWERED — turn Bluetooth on (or: rfkill unblock bluetooth)", "adapter");
                            ready = false;
                        }
                    }
                }
            }
            Err(e) => {
                println!("{:<16} unknown — {e}", "adapter");
                ready = false;
            }
        }
    }

    // Port.
    let addr = format!("127.0.0.1:{}", args.port);
    match tokio::time::timeout(
        Duration::from_millis(400),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_)) => {
            let ver = fetch_health_version(args.port).await;
            match ver {
                Some(v) => println!(
                    "{:<16} in use — catprinterd {v} already listening",
                    format!("port {}", args.port)
                ),
                None => println!(
                    "{:<16} in use — something else is listening",
                    format!("port {}", args.port)
                ),
            }
        }
        _ => println!("{:<16} free", format!("port {}", args.port)),
    }

    println!(
        "{:<16} {}",
        "result",
        if ready { "READY" } else { "NOT READY" }
    );
    if ready {
        0
    } else {
        1
    }
}

async fn fetch_health_version(port: u16) -> Option<String> {
    let out = tokio::time::timeout(Duration::from_secs(2), async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .ok()?;
        s.write_all(format!("GET /health HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes())
            .await
            .ok()?;
        let mut buf = String::new();
        s.read_to_string(&mut buf).await.ok()?;
        Some(buf)
    })
    .await
    .ok()??;
    let body = out.split("\r\n\r\n").nth(1)?;
    let v: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    v.get("version")?.as_str().map(String::from)
}
