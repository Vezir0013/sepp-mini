//! Minimaler, allokationsarmer SSE-Decoder (Server-Sent Events).
//!
//! Bewusst ohne externe Crate: byte-gepuffert, schneidet komplette Zeilen an `\n` und
//! liefert pro leerer Zeile die zusammengesetzten `data:`-Payloads. `event:`/`id:`/
//! Kommentarzeilen werden ignoriert (wir routen über das `type`-Feld im JSON).
//!
//! Mehrbyte-UTF-8 kann nie an `\n` (0x0A) zerschnitten werden — Fortsetzungsbytes sind
//! ≥ 0x80 —, daher ist „Byte puffern, an `\n` splitten, dann decodieren" korrekt, auch
//! wenn ein Zeichen über eine Chunk-Grenze fällt.

/// Obergrenze für eine einzelne (noch nicht durch `\n` terminierte) Pufferzeile.
/// Schützt gegen unbegrenztes Wachstum bei kaputten/bösartigen Streams ohne Zeilenende.
const MAX_BUFFERED_LINE: usize = 64 * 1024 * 1024;

/// Obergrenze für einen zusammengesetzten Event (Summe der `data:`-Zeilen, bevor eine Leerzeile
/// kommt). Schützt gegen Streams, die endlos `data:`-Zeilen ohne Event-Ende senden (OOM).
const MAX_EVENT_BYTES: usize = 64 * 1024 * 1024;

/// Inkrementeller SSE-Decoder.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    /// Bis hierhin wurde `buf` bereits nach `\n` durchsucht (kein Re-Scan → O(n) gesamt).
    scanned: usize,
    data_lines: Vec<String>,
    /// Laufende Bytegröße von `data_lines` (vermeidet O(n²)-Summierung).
    data_bytes: usize,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Füttert rohe Bytes ein und liefert alle dadurch vollständig gewordenen
    /// Event-Payloads (zusammengesetzte `data:`-Zeilen).
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        // Ab `scanned` weiterscannen — frühere Bytes hatten garantiert kein `\n`.
        let mut i = self.scanned;
        while i < self.buf.len() {
            if self.buf[i] == b'\n' {
                let mut end = i;
                if end > start && self.buf[end - 1] == b'\r' {
                    end -= 1;
                }
                let line = String::from_utf8_lossy(&self.buf[start..end]).into_owned();
                // `line` ist owned → Borrow auf `self.buf` ist beendet, wir dürfen mutieren.
                if line.is_empty() {
                    if !self.data_lines.is_empty() {
                        self.data_bytes = 0;
                        out.push(std::mem::take(&mut self.data_lines).join("\n"));
                    }
                } else if let Some(rest) = line.strip_prefix("data:") {
                    // SSE erlaubt genau ein optionales Leerzeichen nach dem Doppelpunkt.
                    let rest = rest.strip_prefix(' ').unwrap_or(rest);
                    self.data_bytes += rest.len() + 1;
                    self.data_lines.push(rest.to_string());
                    // Event ohne Ende, das ins Unendliche wächst → verwerfen (Defense-in-depth).
                    if self.data_bytes > MAX_EVENT_BYTES {
                        self.data_lines.clear();
                        self.data_bytes = 0;
                    }
                }
                // event:/id:/retry:/Kommentare (":...") werden ignoriert.
                start = i + 1;
            }
            i += 1;
        }
        self.buf.drain(..start);
        self.scanned = self.buf.len();

        // Defense-in-depth: niemals unbegrenzt puffern.
        if self.buf.len() > MAX_BUFFERED_LINE {
            self.buf.clear();
            self.scanned = 0;
        }
        out
    }

    /// Schließt den Stream ab: dispatcht einen evtl. noch offenen Event-Block
    /// (falls die letzte leere Zeile fehlt).
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.data_lines.is_empty() {
            self.data_bytes = 0;
            out.push(std::mem::take(&mut self.data_lines).join("\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_event() {
        let mut d = SseDecoder::new();
        let out = d.push(b"data: {\"a\":1}\n\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string()]);
    }

    #[test]
    fn ignores_event_and_comment_lines() {
        let mut d = SseDecoder::new();
        let out = d.push(b": ping\nevent: foo\ndata: x\n\n");
        assert_eq!(out, vec!["x".to_string()]);
    }

    #[test]
    fn split_across_chunks() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: {\"k\":\"va").is_empty());
        assert!(d.push(b"lue\"}").is_empty());
        let out = d.push(b"\n\n");
        assert_eq!(out, vec!["{\"k\":\"value\"}".to_string()]);
    }

    #[test]
    fn multibyte_split_across_chunks() {
        let mut d = SseDecoder::new();
        let s = "über".as_bytes(); // 'ü' = 0xC3 0xBC
        d.push(b"data: ");
        d.push(&s[..1]); // schneidet MITTEN in 'ü' (nur 0xC3)
        let out = d.push(&[&s[1..], b"\n\n"].concat());
        assert_eq!(out, vec!["über".to_string()]);
    }

    #[test]
    fn crlf_line_endings() {
        let mut d = SseDecoder::new();
        let out = d.push(b"data: a\r\n\r\n");
        assert_eq!(out, vec!["a".to_string()]);
    }

    #[test]
    fn many_chunks_without_newline_then_complete() {
        // Datenzeile über viele Chunks gestückelt (kein `\n`) — der Scan-Offset darf
        // nichts verlieren und kein `\n` erst spät erkennen.
        let mut d = SseDecoder::new();
        d.push(b"data: ");
        for _ in 0..1000 {
            assert!(d.push(b"x").is_empty());
        }
        let out = d.push(b"\n\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 1000);
        assert!(out[0].bytes().all(|b| b == b'x'));
    }
}
