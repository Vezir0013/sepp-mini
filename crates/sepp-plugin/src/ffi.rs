//! Die rohen Importe aus dem Modul `env` — nur unter `wasm32` vorhanden. Nativ liefern die
//! Wrapper einen Fehler, damit SDK und Plugin auch ohne wasm32-Target kompilieren und testen.
//!
//! Jeder Import steht hinter dem Feature, das seine Fähigkeit freischaltet: Ein Modul importiert
//! nur, was es benutzt, und das Linker-Gate des Hosts bleibt konsistent (siehe `wit/sepp.wit`).

#[allow(unused_imports)]
use crate::error::{Error, Result};

#[cfg(target_arch = "wasm32")]
mod raw {
    #[link(wasm_import_module = "env")]
    extern "C" {
        pub fn host_log(ptr: i32, len: i32);
        #[cfg(any(feature = "fs-read", feature = "net"))]
        pub fn host_result_read(ptr: i32, cap: i32) -> i32;
        #[cfg(feature = "fs-read")]
        pub fn host_fs_read_bytes(ptr: i32, len: i32) -> i32;
        #[cfg(feature = "net")]
        pub fn host_http(ptr: i32, len: i32) -> i32;
    }
}

// Nur gebraucht, wenn eine Fähigkeit nativ einen Fehler erklären muss.
#[cfg(all(not(target_arch = "wasm32"), any(feature = "fs-read", feature = "net")))]
const NATIVE: &str = "nur unter wasm32 verfügbar";

/// Eine Zeile ins Host-Log.
pub(crate) fn log(message: &str) {
    #[cfg(target_arch = "wasm32")]
    // SAFETY: Zeiger und Länge beschreiben `message` im linearen Speicher; der Host liest nur.
    unsafe {
        raw::host_log(message.as_ptr() as i32, message.len() as i32)
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = message;
}

/// Holt das zuletzt vom Host abgelegte Ergebnis (`n` Bytes) über `host_result_read` ab. Nur mit
/// einer Fähigkeit vorhanden — ein Modul ohne Fähigkeiten importiert auch den Abholweg nicht.
#[cfg(all(target_arch = "wasm32", any(feature = "fs-read", feature = "net")))]
pub(crate) fn fetch(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    // SAFETY: `buf` ist `n` Bytes groß; der Host kopiert höchstens `cap = n` Bytes hinein.
    let got = unsafe { raw::host_result_read(buf.as_mut_ptr() as i32, n as i32) };
    if got < 0 {
        return Err(Error::new(
            "host_result_read: Zielpuffer liegt außerhalb des Speichers",
        ));
    }
    buf.truncate(got as usize);
    Ok(buf)
}

#[cfg(all(not(target_arch = "wasm32"), any(feature = "fs-read", feature = "net")))]
pub(crate) fn fetch(_n: usize) -> Result<Vec<u8>> {
    Err(Error::new(format!("host_result_read: {NATIVE}")))
}

/// `host_fs_read_bytes`: Eingabe-JSON hinein, Rückgabewert des Hosts heraus.
#[cfg(feature = "fs-read")]
pub(crate) fn fs_read_bytes(input: &[u8]) -> Result<i32> {
    #[cfg(target_arch = "wasm32")]
    {
        // SAFETY: Zeiger und Länge beschreiben `input`; der Host liest nur.
        Ok(unsafe { raw::host_fs_read_bytes(input.as_ptr() as i32, input.len() as i32) })
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = input;
        Err(Error::new(format!("host_fs_read_bytes: {NATIVE}")))
    }
}

/// `host_http`: Request-JSON hinein, Rückgabewert des Hosts heraus.
#[cfg(feature = "net")]
pub(crate) fn http(input: &[u8]) -> Result<i32> {
    #[cfg(target_arch = "wasm32")]
    {
        // SAFETY: Zeiger und Länge beschreiben `input`; der Host liest nur.
        Ok(unsafe { raw::host_http(input.as_ptr() as i32, input.len() as i32) })
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = input;
        Err(Error::new(format!("host_http: {NATIVE}")))
    }
}
