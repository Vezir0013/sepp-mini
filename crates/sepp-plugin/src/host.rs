//! Der Zugang zum Host: Log immer, Dateien und Netz nur mit dem passenden Feature.

/// Der Host, wie ihn das Werkzeug sieht. Wird vom Makro je Aufruf bereitgestellt; hält keinen
/// Zustand (die Instanz lebt nur einen Aufruf lang).
#[derive(Debug, Clone, Copy, Default)]
pub struct Host {
    _priv: (),
}

impl Host {
    /// Eine Zeile ins Log des Hosts (stderr, tracing-Target `wasm`). Immer verfügbar; nativ ohne
    /// Wirkung.
    pub fn log(&self, message: &str) {
        crate::ffi::log(message);
    }

    /// Dateizugriff — nur mit dem Feature `fs-read`, das zugleich den Import `host_fs_read_bytes`
    /// freischaltet. Das Manifest muss `fs_read` (oder `fs_write`) anfordern, sonst lädt das
    /// Modul nicht.
    #[cfg(feature = "fs-read")]
    pub fn fs(&self) -> crate::fs::Fs {
        crate::fs::Fs::new()
    }

    /// HTTP über den Host — nur mit dem Feature `net`, das zugleich den Import `host_http`
    /// freischaltet. Das Manifest muss `net` anfordern. **Provisorisch:** Der Host liefert heute
    /// noch einen Fehler („noch nicht implementiert"); Stufe 3 setzt die Funktion um.
    #[cfg(feature = "net")]
    pub fn http(&self) -> crate::http::Http {
        crate::http::Http::new()
    }
}
