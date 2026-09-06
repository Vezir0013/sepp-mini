//! Der Fehlertyp des SDK: ein Text. Mehr braucht ein Plugin nicht — was `Err` wird, landet als
//! `is_error`-Ergebnis beim Modell, und das kann mit einem Satz etwas anfangen, mit einem
//! Enum nicht.

use std::fmt;

/// Ein Fehler mit Erklärung. Entsteht aus `String`, `&str`, JSON- und UTF-8-Fehlern per `?`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    /// Ein Fehler mit diesem Text.
    pub fn new(message: impl Into<String>) -> Self {
        Error(message.into())
    }

    /// Der Text.
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error(s.to_owned())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error(format!("JSON: {e}"))
    }
}

impl From<std::string::FromUtf8Error> for Error {
    fn from(e: std::string::FromUtf8Error) -> Self {
        Error(format!("kein gültiges UTF-8: {e}"))
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(e: std::str::Utf8Error) -> Self {
        Error(format!("kein gültiges UTF-8: {e}"))
    }
}

/// Das Ergebnis eines Plugin-Schritts; der Fehlertyp ist per Default [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_keep_the_explanation() {
        let e: Error = "kaputt".into();
        assert_eq!(e.to_string(), "kaputt");
        let e: Error = String::from("auch").into();
        assert_eq!(e.message(), "auch");

        let json_err = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let e: Error = json_err.into();
        assert!(e.message().starts_with("JSON: "), "{e}");

        let utf8 = String::from_utf8(vec![0xff]).unwrap_err();
        let e: Error = utf8.into();
        assert!(e.message().contains("UTF-8"), "{e}");
    }

    #[test]
    fn question_mark_works_with_the_default_result() {
        fn parse() -> Result<serde_json::Value> {
            Ok(serde_json::from_str("{")?)
        }
        assert!(parse().is_err());
    }
}
