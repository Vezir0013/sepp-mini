//! Dateizugriff über den Host (Feature `fs-read`).
//!
//! Gelesen wird **roh** über `host_fs_read_bytes` — ein PDF kommt als PDF an, nicht als
//! `from_utf8_lossy`-Müll. Der Host prüft den Pfad gegen dieselbe Policy wie `read`/`write`/`edit`
//! des Agenten; ein Plugin kommt also nicht weiter als der Agent selbst.

use crate::error::Result;
use crate::{__abi, ffi};

/// Der Dateizugriff des Hosts. Über [`crate::Host::fs`].
#[derive(Debug, Clone, Copy)]
pub struct Fs {
    _priv: (),
}

impl Fs {
    pub(crate) fn new() -> Self {
        Fs { _priv: () }
    }

    /// Liest eine Datei roh. Der Pfad wird wie bei den eingebauten Tools aufgelöst (relativ zum
    /// Arbeitsverzeichnis von sepp, `~` wird expandiert); höchstens 16 MiB. Ein verweigerter
    /// oder fehlender Pfad ist ein `Err` mit der Erklärung des Hosts, kein Trap.
    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        let input = serde_json::json!({ "path": path }).to_string();
        let n = ffi::fs_read_bytes(input.as_bytes())?;
        __abi::decode_raw(n, ffi::fetch)
    }

    /// Liest eine Datei als Text — strikt UTF-8; ungültige Bytes sind ein Fehler. Wer
    /// verlustbehaftet lesen will, nimmt [`Fs::read`] und `String::from_utf8_lossy`.
    pub fn read_to_string(&self, path: &str) -> Result<String> {
        Ok(String::from_utf8(self.read(path)?)?)
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn native_stub_explains_itself() {
        let e = Fs::new()
            .read("/etc/hostname")
            .expect_err("nativ kein Host");
        assert!(e.message().contains("wasm32"), "{e}");
    }
}
