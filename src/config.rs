//! Command line / environment configuration (clap). Every `serve` flag also reads `CATPRINTER_*`.

use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "catprinterd",
    version,
    about = "IPP Everywhere driver for Bluetooth cat printers",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
    /// `catprinterd` with no subcommand runs `serve`; these are its flags.
    #[command(flatten)]
    pub serve: ServeArgs,
    /// Log level / filter (tracing EnvFilter syntax), e.g. info, debug, catprinterd::ble=trace
    #[arg(long, global = true, env = "CATPRINTER_LOG", default_value = "info")]
    pub log_level: String,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Run the IPP printer daemon (default).
    Serve(ServeArgs),
    /// Health check: D-Bus, bluetoothd, adapters, port, plus read-only BlueZ/USB host facts. Exit 1 when not ready.
    Check(CheckArgs),
    /// Connect to the printer and report model / battery / paper.
    Status(BleArgs),
    /// Print an image (PNG/JPEG) or a PWG raster file directly over Bluetooth (bring-up tool).
    Print(PrintArgs),
    /// Show the page headers of a PWG/CUPS raster file.
    Inspect { file: PathBuf },
    /// (root) Ensure the CUPS queue exists and points at this daemon (`lpadmin -m everywhere`).
    EnsureQueue(EnsureQueueArgs),
    /// Discover the printer, force an LE-only BlueZ object, Trust it, remember the MAC.
    Adopt(AdoptArgs),
    /// Report whether a connect would go LE, whether the queue/daemon/adapter are ready.
    Doctor(DoctorArgs),
}

#[derive(Args, Debug, Clone)]
pub struct BleArgs {
    /// Printer MAC address or advertised name (default: any known cat printer, strongest signal).
    #[arg(long, env = "CATPRINTER_DEVICE")]
    pub device: Option<String>,
    /// Force a protocol family instead of autodetecting.
    #[arg(long, env = "CATPRINTER_MODEL", value_enum, default_value_t = ModelArg::Auto)]
    pub model: ModelArg,
    /// Bluetooth adapter (hci0, hci1, …); default: first powered adapter.
    #[arg(long, env = "CATPRINTER_ADAPTER")]
    pub adapter: Option<String>,
    /// One row per BLE write with extra pacing (weak links).
    #[arg(long, env = "CATPRINTER_SLOW", default_value_t = false)]
    pub slow: bool,
    /// Milliseconds between bulk writes.
    #[arg(long, env = "CATPRINTER_PACING_MS", default_value_t = 8)]
    pub pacing_ms: u64,
    /// How to receive notifications: BlueZ PropertiesChanged (props) or AcquireNotify fd (acquire).
    #[arg(long, env = "CATPRINTER_NOTIFY_MODE", value_enum, default_value_t = NotifyMode::Props)]
    pub notify_mode: NotifyMode,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelArg {
    Auto,
    Mxw01,
    Classic,
}

impl ModelArg {
    pub fn family(self) -> Option<crate::models::Family> {
        match self {
            ModelArg::Auto => None,
            ModelArg::Mxw01 => Some(crate::models::Family::Mxw01),
            ModelArg::Classic => Some(crate::models::Family::Classic),
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyMode {
    Props,
    Acquire,
}

#[derive(Args, Debug, Clone)]
pub struct ServeArgs {
    /// Address to bind (loopback only is the design; the queue URI must match).
    #[arg(long, env = "CATPRINTER_BIND", default_value = "127.0.0.1")]
    pub bind: IpAddr,
    /// TCP port for IPP/HTTP.
    #[arg(long, env = "CATPRINTER_PORT", default_value_t = 8095)]
    pub port: u16,
    /// Seconds a job keeps retrying to reach the printer before it is aborted.
    #[arg(long, env = "CATPRINTER_PRINTER_WAIT", default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
    pub printer_wait: u64,
    /// Print every job to PNG files in DIR instead of Bluetooth (testing).
    #[arg(long, env = "CATPRINTER_FAKE_PRINTER")]
    pub fake_printer: Option<PathBuf>,
    /// Max jobs waiting in the queue before Print-Job answers server-error-busy.
    #[arg(long, env = "CATPRINTER_QUEUE_MAX", default_value_t = 16, value_parser = clap::value_parser!(u64).range(1..=1000))]
    pub queue_max: u64,
    /// Max accepted document size in MiB.
    #[arg(long, env = "CATPRINTER_MAX_DOCUMENT_MB", default_value_t = 64, value_parser = clap::value_parser!(u64).range(1..=1024))]
    pub max_document_mb: u64,
    /// Max total strip length in lines (203 lines ≈ 25.4 mm); longer jobs are rejected, never truncated.
    #[arg(long, env = "CATPRINTER_MAX_LINES", default_value_t = 8000, value_parser = clap::value_parser!(u32).range(100..=65535))]
    pub max_lines: u32,
    /// Max lines per print request (segment).
    #[arg(long, env = "CATPRINTER_MAX_LINES_PER_REQUEST", default_value_t = 4000, value_parser = clap::value_parser!(u32).range(90..=65535))]
    pub max_lines_per_request: u32,
    /// Max copies honoured per job.
    #[arg(long, env = "CATPRINTER_MAX_COPIES", default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=99))]
    pub max_copies: u32,
    /// Advertised raster resolutions (dpi). "203" native; "203,406" makes CUPS render Normal/High at 406 dpi.
    #[arg(
        long,
        env = "CATPRINTER_RESOLUTIONS",
        default_value = "203",
        value_delimiter = ','
    )]
    pub resolutions: Vec<u32>,
    /// Advertise on Avahi (loopback _ipp._tcp).
    #[arg(long, env = "CATPRINTER_DNSSD", value_enum, default_value_t = OnOff::On)]
    pub dnssd: OnOff,
    /// DNS-SD service name.
    #[arg(long, env = "CATPRINTER_DNSSD_NAME", default_value = "Cat Printer")]
    pub dnssd_name: String,
    /// IPP printer-name.
    #[arg(long, env = "CATPRINTER_PRINTER_NAME", default_value = "CatPrinter")]
    pub printer_name: String,
    /// CUPS queue name whose printer-uuid we adopt (for DNS-SD de-duplication).
    #[arg(long, env = "CATPRINTER_QUEUE", default_value = "CatPrinter")]
    pub queue: String,
    /// Fixed printer-uuid (default: adopt the CUPS queue's, else v5 of machine-id:port).
    #[arg(long, env = "CATPRINTER_UUID")]
    pub uuid: Option<String>,
    /// printer-location text.
    #[arg(
        long,
        env = "CATPRINTER_LOCATION",
        default_value = "Bluetooth, wherever the cat printer is"
    )]
    pub location: String,
    /// Directory for small persistent state (the job-id counter). systemd's `$STATE_DIRECTORY`
    /// (/var/lib/catprinter) is used when unset.
    #[arg(long, env = "CATPRINTER_STATE_DIR")]
    pub state_dir: Option<PathBuf>,
    #[command(flatten)]
    pub ble: BleArgs,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnOff {
    On,
    Off,
}

#[derive(Args, Debug, Clone)]
pub struct AdoptArgs {
    /// Printer MAC (or advertised name). Omit to pick the first cat printer that advertises.
    #[arg(long, env = "CATPRINTER_DEVICE")]
    pub device: Option<String>,
    /// Print whether a printer is adopted on this host (no scan).
    #[arg(long, default_value_t = false)]
    pub status: bool,
    /// Drop the persisted MAC and remove the trusted BlueZ record (used by uninstall).
    #[arg(long, default_value_t = false)]
    pub forget: bool,
    /// Bluetooth adapter (hci0, hci1, …).
    #[arg(long, env = "CATPRINTER_ADAPTER")]
    pub adapter: Option<String>,
    /// Directory for the adopted-MAC file (default: `$STATE_DIRECTORY` / `/var/lib/catprinter`).
    #[arg(long, env = "CATPRINTER_STATE_DIR")]
    pub state_dir: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct DoctorArgs {
    /// Emit JSON instead of a table (integrator-parseable).
    #[arg(long, default_value_t = false)]
    pub json: bool,
    #[arg(long, env = "CATPRINTER_PORT", default_value_t = 8095)]
    pub port: u16,
    #[arg(long, env = "CATPRINTER_QUEUE", default_value = "CatPrinter")]
    pub queue: String,
    #[arg(long, env = "CATPRINTER_ADAPTER")]
    pub adapter: Option<String>,
    #[arg(long, env = "CATPRINTER_STATE_DIR")]
    pub state_dir: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct CheckArgs {
    #[arg(long, env = "CATPRINTER_PORT", default_value_t = 8095)]
    pub port: u16,
    #[arg(long, env = "CATPRINTER_ADAPTER")]
    pub adapter: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct PrintArgs {
    /// PNG, JPEG or PWG raster file.
    pub file: PathBuf,
    /// Print style: default, picture, text, document (aliases: normal, high, draft, doc).
    #[arg(short = 'q', long, default_value = "default")]
    pub quality: String,
    /// Tone: blackwhite (1 bpp) or grayscale (16-level on MXW01). Independent of style.
    #[arg(long, default_value = "blackwhite")]
    pub tone: String,
    /// Force 1-bit even if --tone grayscale.
    #[arg(long, default_value_t = false)]
    pub bi_level: bool,
    /// Treat pages as sheets (no trim, whole page shrunk) instead of tape.
    #[arg(long, default_value_t = false)]
    pub sheet: bool,
    /// Override the dither (floyd-steinberg, atkinson, threshold, mean, none).
    #[arg(long)]
    pub dither: Option<String>,
    /// Override the burn intensity (0-255, e.g. 0x5D).
    #[arg(long, value_parser = parse_u8_auto)]
    pub intensity: Option<u8>,
    /// Number of copies.
    #[arg(short = 'n', long, default_value_t = 1)]
    pub copies: u32,
    /// Do not print; write what would be printed to this PNG.
    #[arg(long)]
    pub preview_only: Option<PathBuf>,
    /// Do not rotate 180° (top of the image comes out first).
    #[arg(long, default_value_t = false)]
    pub top_first: bool,
    #[arg(long, default_value_t = 8000)]
    pub max_lines: u32,
    #[command(flatten)]
    pub ble: BleArgs,
}

fn parse_u8_auto(s: &str) -> Result<u8, String> {
    let s = s.trim();
    let v = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16)
    } else {
        s.parse::<u32>()
    }
    .map_err(|e| e.to_string())?;
    u8::try_from(v).map_err(|_| "must be 0..=255".to_string())
}

#[derive(Args, Debug, Clone)]
pub struct EnsureQueueArgs {
    #[arg(long, env = "CATPRINTER_QUEUE", default_value = "CatPrinter")]
    pub queue: String,
    #[arg(long, env = "CATPRINTER_PORT", default_value_t = 8095)]
    pub port: u16,
    #[arg(
        long,
        env = "CATPRINTER_LOCATION",
        default_value = "Bluetooth, wherever the cat printer is"
    )]
    pub location: String,
    /// Remove the queue instead of creating it.
    #[arg(long, default_value_t = false)]
    pub remove: bool,
    /// Seconds to wait for the daemon and cupsd before giving up.
    #[arg(long, default_value_t = 60)]
    pub wait: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let mut v = vec!["catprinterd"];
        v.extend_from_slice(args);
        Cli::try_parse_from(v)
    }

    #[test]
    fn defaults_are_sane() {
        let c = parse(&[]).unwrap();
        assert_eq!(c.serve.port, 8095);
        assert_eq!(c.serve.max_copies, 10);
        assert_eq!(c.serve.max_document_mb, 64);
        assert_eq!(c.serve.printer_wait, 120);
    }

    #[test]
    fn rejects_absurd_values() {
        assert!(parse(&["--max-copies", "3000000000"]).is_err());
        assert!(parse(&["--max-copies", "0"]).is_err());
        assert!(parse(&["--max-document-mb", "999999"]).is_err());
        assert!(parse(&["--max-document-mb", "0"]).is_err());
        assert!(parse(&["--max-lines", "0"]).is_err());
        assert!(parse(&["--max-lines", "70000"]).is_err());
        assert!(parse(&["--max-lines-per-request", "10"]).is_err());
        assert!(parse(&["--queue-max", "0"]).is_err());
        assert!(parse(&["--printer-wait", "0"]).is_err());
        // sane values still parse
        assert!(parse(&[
            "--max-copies",
            "5",
            "--max-document-mb",
            "32",
            "--max-lines",
            "4000"
        ])
        .is_ok());
    }

    #[test]
    fn adopt_and_doctor_flags_parse() {
        match parse(&["adopt", "--status"]).unwrap().cmd {
            Some(Cmd::Adopt(a)) => {
                assert!(a.status);
                assert!(!a.forget);
                assert!(a.device.is_none());
            }
            other => panic!("expected adopt --status, got {other:?}"),
        }
        match parse(&["adopt", "--device", "AA:BB:CC:DD:EE:FF"])
            .unwrap()
            .cmd
        {
            Some(Cmd::Adopt(a)) => {
                assert_eq!(a.device.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
                assert!(!a.status);
            }
            other => panic!("expected adopt --device, got {other:?}"),
        }
        match parse(&["doctor", "--json"]).unwrap().cmd {
            Some(Cmd::Doctor(a)) => {
                assert!(a.json);
                assert_eq!(a.port, 8095);
            }
            other => panic!("expected doctor --json, got {other:?}"),
        }
    }
}
