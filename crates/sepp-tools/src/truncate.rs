//! Ausgabe-Trunkierung (Pflicht für alle Tools).
//!
//! Arbeitet auf **Byte-Bereichen des Originals** und gibt bei „passt" den Input unverändert
//! zurück (keine Re-Konstruktion via `lines().join`, die Zeilenenden/Trailing-Newline
//! zerstören würde).

/// Default-Byte-Grenze (50 KiB).
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Default-Zeilen-Grenze.
pub const DEFAULT_MAX_LINES: usize = 2000;

/// Ergebnis einer Trunkierung.
#[derive(Debug, Clone)]
pub struct Truncated {
    pub content: String,
    pub truncated: bool,
    pub shown_lines: usize,
    pub total_lines: usize,
    pub shown_bytes: usize,
    pub total_bytes: usize,
}

/// Kürzt die Text-Blöcke eines Tool-Ergebnisses auf die Default-Grenzen — für Quellen, die nicht
/// selbst kürzen (MCP, WASM). Nicht-Text-Blöcke (z. B. Bilder) bleiben unverändert.
///
/// Das Budget gilt für das **ganze Ergebnis**, nicht je Block: Sonst passierten 200 Blöcke à
/// 50 KiB jeder für sich die Grenze und landeten als 10 MB im Kontextfenster. Ist es erschöpft,
/// werden die restlichen Text-Blöcke durch **einen** Hinweis ersetzt.
pub fn truncate_content_blocks(
    blocks: Vec<sepp_core::ContentBlock>,
) -> Vec<sepp_core::ContentBlock> {
    let mut remaining_bytes = DEFAULT_MAX_BYTES;
    let mut remaining_lines = DEFAULT_MAX_LINES;
    let mut dropped_blocks = 0usize;
    let mut dropped_bytes = 0usize;
    let mut out = Vec::with_capacity(blocks.len());

    for b in blocks {
        match b {
            sepp_core::ContentBlock::Text { text } => {
                if remaining_bytes == 0 || remaining_lines == 0 {
                    dropped_blocks += 1;
                    dropped_bytes += text.len();
                    continue;
                }
                let t = truncate_tail(&text, remaining_lines, remaining_bytes);
                remaining_bytes -= t.shown_bytes.min(remaining_bytes);
                remaining_lines -= t.shown_lines.min(remaining_lines);
                let note = t.note();
                let mut s = t.content;
                if let Some(note) = note {
                    s.push_str(&note);
                }
                out.push(sepp_core::ContentBlock::Text { text: s });
            }
            other => out.push(other),
        }
    }

    if dropped_blocks > 0 {
        out.push(sepp_core::ContentBlock::text(format!(
            "[Output gekürzt: {dropped_blocks} weitere Text-Blöcke ({dropped_bytes} Bytes) \
             entfallen — das Budget von {DEFAULT_MAX_BYTES} Bytes / {DEFAULT_MAX_LINES} Zeilen \
             gilt für das ganze Tool-Ergebnis]"
        )));
    }
    out
}

impl Truncated {
    /// Hinweis-Suffix fürs Modell, falls gekürzt wurde.
    pub fn note(&self) -> Option<String> {
        if !self.truncated {
            return None;
        }
        Some(format!(
            "\n\n[Output gekürzt: {} von {} Zeilen ({} von {} Bytes)]",
            self.shown_lines, self.total_lines, self.shown_bytes, self.total_bytes
        ))
    }
}

/// Byte-Offset direkt hinter dem `n`-ten `\n`; `s.len()`, wenn es so viele nicht gibt.
fn offset_after_newline(s: &str, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let mut seen = 0;
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            seen += 1;
            if seen == n {
                return i + 1;
            }
        }
    }
    s.len()
}

/// Schneidet `take` Zeilen ab Zeile `skip` heraus — als **Byte-Bereich des Originals**.
///
/// Damit bleiben `\r\n` und eine abschließende Newline erhalten. Eine Rekonstruktion über
/// `lines().join("\n")` täte das nicht: Sie normalisiert CRLF zu LF, und das Modell verlöre
/// danach jede Chance, einen kopierten `old_string` in einer CRLF-Datei wiederzufinden.
/// Zeilensemantik wie [`count_lines`].
pub fn line_slice(s: &str, skip: usize, take: Option<usize>) -> &str {
    let start = offset_after_newline(s, skip);
    let end = match take {
        Some(n) => offset_after_newline(&s[start..], n) + start,
        None => s.len(),
    };
    &s[start..end]
}

/// Editor-gerechte Zeilenzahl: Anzahl `\n` plus 1, falls nicht mit `\n` endend.
/// `""` → 0, `"a\n"` → 1, `"a\nb"` → 2, `"a\nb\n"` → 2.
fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let nl = s.bytes().filter(|&b| b == b'\n').count();
    if s.ends_with('\n') {
        nl
    } else {
        nl + 1
    }
}

/// Größter Char-Boundary `<= max`.
fn floor_boundary(s: &str, max: usize) -> usize {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Kleinster Char-Boundary `>= min`.
fn ceil_boundary(s: &str, min: usize) -> usize {
    let mut start = min.min(s.len());
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    start
}

/// Behält den **Anfang** (Datei-Reads, Suchergebnisse). Zeilenenden bleiben byte-exakt erhalten.
pub fn truncate_head(s: &str, max_lines: usize, max_bytes: usize) -> Truncated {
    let total_bytes = s.len();
    let total_lines = count_lines(s);

    let line_cut = offset_after_newline(s, max_lines);
    let truncated_by_lines = line_cut < s.len();

    let mut cut = line_cut;
    let truncated_by_bytes = cut > max_bytes;
    if truncated_by_bytes {
        cut = floor_boundary(s, max_bytes);
    }

    finalize(
        s[..cut].to_string(),
        truncated_by_lines || truncated_by_bytes,
        total_lines,
        total_bytes,
    )
}

/// Behält das **Ende** (Logs, Kommando-Output). Zeilenenden bleiben byte-exakt erhalten.
pub fn truncate_tail(s: &str, max_lines: usize, max_bytes: usize) -> Truncated {
    let total_bytes = s.len();
    let total_lines = count_lines(s);

    // Die ersten (total_lines - max_lines) Zeilen verwerfen.
    let drop = total_lines.saturating_sub(max_lines);
    let line_start = offset_after_newline(s, drop);
    let truncated_by_lines = line_start > 0;

    let mut start = line_start;
    let truncated_by_bytes = s.len() - start > max_bytes;
    if truncated_by_bytes {
        start = ceil_boundary(s, s.len() - max_bytes).max(line_start);
    }

    finalize(
        s[start..].to_string(),
        truncated_by_lines || truncated_by_bytes,
        total_lines,
        total_bytes,
    )
}

fn finalize(content: String, truncated: bool, total_lines: usize, total_bytes: usize) -> Truncated {
    let shown_bytes = content.len();
    let shown_lines = count_lines(&content);
    Truncated {
        content,
        truncated,
        shown_lines,
        total_lines,
        shown_bytes,
        total_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_truncation_returns_input_verbatim() {
        // Inklusive Trailing-Newline und CRLF — beides muss erhalten bleiben.
        for s in ["a\nb\nc", "a\nb\nc\n", "x\r\ny\r\n", "", "\n"] {
            let t = truncate_head(s, 10, 1000);
            assert!(!t.truncated, "{s:?} sollte nicht gekürzt sein");
            assert_eq!(t.content, s, "Inhalt muss byte-exakt sein für {s:?}");
            assert!(t.note().is_none());
            let t2 = truncate_tail(s, 10, 1000);
            assert_eq!(t2.content, s);
        }
    }

    fn text_of(b: &sepp_core::ContentBlock) -> &str {
        match b {
            sepp_core::ContentBlock::Text { text } => text,
            _ => panic!("kein Text-Block"),
        }
    }

    #[test]
    fn line_slice_keeps_crlf_and_trailing_newline() {
        let s = "a\r\nb\r\nc\r\n";
        assert_eq!(line_slice(s, 1, Some(1)), "b\r\n");
        assert_eq!(line_slice(s, 0, None), s);
        assert_eq!(line_slice(s, 2, None), "c\r\n");
    }

    #[test]
    fn line_slice_handles_out_of_range() {
        let s = "a\nb";
        assert_eq!(line_slice(s, 0, Some(99)), "a\nb");
        assert_eq!(line_slice(s, 2, None), "");
        assert_eq!(line_slice(s, 99, Some(1)), "");
        assert_eq!(line_slice("", 0, None), "");
    }

    #[test]
    fn line_slice_agrees_with_count_lines() {
        let s = "eins\nzwei\ndrei\n";
        for skip in 0..=count_lines(s) {
            let rest = line_slice(s, skip, None);
            assert_eq!(count_lines(rest), count_lines(s) - skip, "skip={skip}");
        }
    }

    #[test]
    fn block_budget_is_shared_across_the_whole_result() {
        let big = "x".repeat(DEFAULT_MAX_BYTES);
        let blocks: Vec<_> = (0..200)
            .map(|_| sepp_core::ContentBlock::text(big.clone()))
            .collect();
        let out = truncate_content_blocks(blocks);
        let total: usize = out.iter().map(|b| text_of(b).len()).sum();
        // Ohne das gemeinsame Budget wären das 10 MB.
        assert!(total < 2 * DEFAULT_MAX_BYTES, "{total}");
        assert!(text_of(out.last().unwrap()).contains("weitere Text-Blöcke"));
    }

    #[test]
    fn small_result_passes_through_verbatim() {
        let blocks = vec![
            sepp_core::ContentBlock::text("hallo"),
            sepp_core::ContentBlock::text("welt\n"),
        ];
        let out = truncate_content_blocks(blocks);
        assert_eq!(out.len(), 2);
        assert_eq!(text_of(&out[0]), "hallo");
        assert_eq!(text_of(&out[1]), "welt\n");
    }

    #[test]
    fn non_text_blocks_survive_at_their_place() {
        let img = sepp_core::ContentBlock::Image {
            source: sepp_core::ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "AA==".into(),
            },
        };
        let blocks = vec![
            sepp_core::ContentBlock::text("x".repeat(DEFAULT_MAX_BYTES * 2)),
            img,
            sepp_core::ContentBlock::text("wird verworfen"),
        ];
        let out = truncate_content_blocks(blocks);
        // Das Bild bleibt an seiner Stelle, obwohl das Budget davor schon erschöpft war.
        assert!(matches!(out[1], sepp_core::ContentBlock::Image { .. }));
    }

    #[test]
    fn count_lines_matches_editor_semantics() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("a"), 1);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines("a\nb\n"), 2);
    }

    #[test]
    fn head_limits_lines_and_keeps_terminators() {
        let s = (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let t = truncate_head(&s, 3, 1_000_000);
        assert!(t.truncated);
        assert_eq!(t.content, "0\n1\n2\n"); // drei Zeilen inkl. trennendem \n
        assert_eq!(t.shown_lines, 3);
        assert_eq!(t.total_lines, 100);
        assert!(t.note().is_some());
    }

    #[test]
    fn tail_limits_lines_and_keeps_end() {
        let s = (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let t = truncate_tail(&s, 3, 1_000_000);
        assert!(t.truncated);
        assert_eq!(t.content, "97\n98\n99");
        assert_eq!(t.shown_lines, 3);
    }

    #[test]
    fn total_lines_correct_for_trailing_newline() {
        // 5 Zeilen, mit Trailing-Newline → total_lines muss 5 sein (nicht 4).
        let s = "1\n2\n3\n4\n5\n";
        let t = truncate_head(s, 2, 1_000_000);
        assert_eq!(t.total_lines, 5);
        assert_eq!(t.content, "1\n2\n");
    }

    #[test]
    fn byte_limit_respects_char_boundaries() {
        let s = " üüüüü"; // Leerzeichen + 5x 'ü' (je 2 Bytes)
        let t = truncate_head(s, 100, 4);
        assert!(t.truncated);
        assert!(t.content.is_char_boundary(t.content.len()));
        assert!(t.content.len() <= 4);
    }

    #[test]
    fn tail_byte_limit_keeps_suffix() {
        let s = "0123456789";
        let t = truncate_tail(s, 100, 4);
        assert!(t.truncated);
        assert_eq!(t.content, "6789");
    }
}
