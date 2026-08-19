//! catprinterd — IPP Everywhere driver for Bluetooth "cat" thermal printers.
//!
//! Layering (see the plan / README):
//!   http  → ipp (Everywhere attributes, ops) → engine (queue, single worker, hold-and-wait)
//!         → raster (PWG decode) → render (trim/fit/dither/pack)
//!         → printer { Ble(models over ble) | Fake }
//!   dnssd (Avahi on loopback), cupsq (adopt the CUPS queue's printer-uuid)

pub mod adopt;
pub mod ble;
pub mod commands;
pub mod config;
pub mod cupsq;
pub mod dnssd;
pub mod doctor;
pub mod engine;
pub mod http;
pub mod ipp;
pub mod models;
pub mod printer;
pub mod protocol;
pub mod raster;
pub mod render;

/// Crate version, single-sourced from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_version_is_0_3_2() {
        assert_eq!(crate::VERSION, "0.3.2");
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.3.2");
    }
}
