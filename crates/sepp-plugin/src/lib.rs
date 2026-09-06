//! `sepp-plugin` — das Guest-SDK für WASM-Plugins von sepp mini (Tier 2).
//!
//! Der Autor schreibt eine Funktion, kein Protokoll:
//!
//! ```ignore
//! use sepp_plugin::prelude::*;
//!
//! #[derive(Deserialize, JsonSchema)]
//! struct Args {
//!     /// Der zu vermessende Text.
//!     text: String,
//! }
//!
//! #[sepp_plugin::tool(desc = "Zählt die Wörter eines Textes.")]
//! fn woerter(args: Args, host: &Host) -> Result<ToolResult> {
//!     host.log("los geht's");
//!     let n = args.text.split_whitespace().count();
//!     Ok(ToolResult::text(format!("{n} Wörter")).with_details(json!({ "n": n })))
//! }
//! ```
//!
//! Das Attribut [`tool`] erzeugt daraus die Exports des Plugin-ABI 1 (`sepp_alloc`, `sepp_spec`,
//! `sepp_call`; Vertragstext: `wit/sepp.wit` im Repo-Root), das Parameter-Schema entsteht aus
//! `Args` (`schemars`), und Fehler werden zu einem Ergebnis mit `is_error = true` — ein Plugin
//! trappt nie. Genau ein `#[tool]` je Crate; das Crate ist eine `cdylib` für
//! `wasm32-unknown-unknown`.
//!
//! **Fähigkeiten sind Cargo-Features und damit Compile-Gates.** [`Host::fs`] gibt es nur mit dem
//! Feature `fs-read`, [`Host::http`] nur mit `net`. Ein Feature schaltet zugleich den zugehörigen
//! Host-Import frei — das Modul importiert also nur, was es benutzt. Der Host registriert eine
//! gegatete Funktion nur, wenn Manifest **und** `policy.toml [plugin.<name>]` sie gewähren; ein
//! Modul, das sie ohne Gewährung importiert, lädt nicht. Deshalb: ein Feature nur setzen, wenn das
//! Manifest das Recht anfordert.
//!
//! **Nativ** (Tests, Clippy, CI ohne wasm32-Target) kompiliert das SDK ebenfalls: Die Exports
//! entstehen nur unter `target_arch = "wasm32"`, die Host-Fähigkeiten liefern nativ einen Fehler
//! („nur unter wasm32 verfügbar"), `Host::log` tut nichts. Ein Autor testet sein Werkzeug über
//! das vom Makro erzeugte Modul `__sepp_plugin_export` (`spec_json()`, `call_json(&[u8])`).
//!
//! Was der Autor sonst wissen muss: Es gibt keinen Zustand zwischen zwei Aufrufen (der Host
//! verwirft die Instanz nach jedem Aufruf), die Standardbibliothek trägt nur zur Hälfte (keine Uhr,
//! kein Zufall, kein `std::fs` — Dateien nur über [`Host::fs`]), und `serde` sowie `schemars`
//! müssen direkte Dependencies des Plugin-Crates sein, weil ihre Derive-Makros das verlangen.

pub mod prelude;

#[doc(hidden)]
pub mod __abi;

mod error;
mod ffi;
mod host;

#[cfg(feature = "fs-read")]
mod fs;
#[cfg(feature = "net")]
pub mod http;

pub use error::{Error, Result};
#[cfg(feature = "fs-read")]
pub use fs::Fs;
pub use host::Host;
pub use sepp_plugin_macros::tool;

// Die Crates, gegen die das SDK gebaut ist — damit ein Autor dieselben Versionen sieht, ohne sie
// selbst zu pinnen. (Für die Derives braucht er `serde` und `schemars` trotzdem als eigene Deps.)
pub use schemars;
pub use sepp_core;
pub use serde;
pub use serde_json;
