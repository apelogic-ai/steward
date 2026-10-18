use std::io;

use tokio_rustls::rustls::crypto::{CryptoProvider, ring};

/// Selects Steward's process-wide Rustls provider before any TLS client or server is built.
pub fn install() -> Result<(), io::Error> {
    if CryptoProvider::get_default().is_none() {
        let _ = ring::default_provider().install_default();
    }
    if CryptoProvider::get_default().is_some() {
        Ok(())
    } else {
        Err(io::Error::other(
            "install the Steward Rustls crypto provider",
        ))
    }
}
