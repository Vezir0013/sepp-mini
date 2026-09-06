//! Fremden Text sicher anzeigen.
//!
//! Ein Terminal ist kein Textfeld: Es liest Steuerzeichen als Befehle. Ein `ESC` beginnt eine
//! ANSI-Sequenz, die färbt, den Cursor bewegt oder Zeilen löscht; ein Wagenrücklauf springt an
//! den Zeilenanfang, sodass Folgendes das Vorherige überschreibt. Text, der aus einem
//! Paket-Manifest, einem Registry-Index, einem MCP-Server, einem WASM-Plugin oder einem
//! Werkzeug-Ergebnis stammt, gehört niemandem, dem wir vertrauen — und er landet ausgerechnet
//! im Zustimmungsdialog, direkt neben der Frage „Rechte gewähren?".
//!
//! Zwei Angriffe, die dieselbe Ursache haben:
//!
//! * **Überschreiben.** Eine Beschreibung mit Wagenrücklauf löscht die Zeile, in der die Rechte
//!   eines Plugins stehen, und schreibt eine harmlose an ihre Stelle. Der Mensch stimmt etwas
//!   zu, das er nie gesehen hat.
//! * **Trugbild.** Ein Rechts-nach-links-Zeichen kehrt die Anzeige um: `gro.esiob` liest sich
//!   als `boise.org`. Breitenlose Zeichen machen zwei verschiedene Paketnamen optisch gleich.
//!
//! **Ersetzt wird, nicht gelöscht.** Wer unsichtbare Zeichen entfernt, macht einen
//! manipulierten Namen von einem harmlosen ununterscheidbar — genau die Verwechslung, auf die
//! es der Angreifer anlegt. Ein sichtbares Ersatzzeichen sagt dem Menschen dagegen: Hier stand
//! etwas, das nicht hierher gehört.
//!
//! **Wo das nicht nötig ist:** In der TUI nicht — ratatui verwirft Grapheme ohne Breite und
//! filtert Steuerzeichen beim Schreiben in den Puffer. Und überall dort nicht, wo mit `{:?}`
//! formatiert wird: Rusts Debug-Darstellung für Zeichenketten escapt Steuer- und Formatzeichen
//! bereits. Gefährlich ist die einfache Anzeige mit `{}` in `println!`/`eprintln!` — also
//! `sepp pkg`, `sepp audit`, `sepp policy` und die Startmeldungen ohne TUI.

/// Das Zeichen, das an die Stelle eines gefährlichen tritt: `U+FFFD REPLACEMENT CHARACTER`.
/// Jedes Terminal stellt es dar, und es ist die übliche Marke für „hier stand etwas
/// Unlesbares".
pub const DISPLAY_REPLACEMENT: char = '\u{FFFD}';

/// Ist dieses Zeichen in einer Terminalzeile gefährlich oder unsichtbar?
///
/// Neben den Steuerzeichen erfasst die Prüfung die beiden Klassen, die nichts steuern, aber
/// täuschen: die Bidi-Formatzeichen (Text visuell umkehren) und die breitenlosen Zeichen
/// (verschiedene Namen gleich aussehen lassen).
fn is_unsafe_for_display(c: char) -> bool {
    matches!(c,
        // C0 (samt Zeilenumbruch und Tabulator) und DEL.
        '\u{0}'..='\u{1F}' | '\u{7F}'
        // C1 — ein zweiter Weg zu denselben Sequenzen.
        | '\u{80}'..='\u{9F}'
        // Bidi: Embedding, Override, Isolate und die beiden Marks.
        | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        // Breitenlos: Zero-Width Space/Non-Joiner/Joiner und BOM.
        | '\u{200B}'..='\u{200D}' | '\u{FEFF}')
}

/// Ist die Zeichenkette so, wie sie ist, gefahrlos anzeigbar?
///
/// Dieselbe Zeichenmenge wie [`sanitize_display`], nur als Frage statt als Umwandlung — für
/// Stellen, die **ablehnen** statt bereinigen: eine URL mit einem Rechts-nach-links-Zeichen im
/// Hostnamen soll gar nicht erst benutzt werden, nicht bloß hübsch angezeigt.
pub fn is_display_safe(s: &str) -> bool {
    !s.chars().any(is_unsafe_for_display)
}

/// Macht fremden Text für eine **einzeilige** Anzeige ungefährlich.
///
/// Zeilenumbrüche und Tabulatoren werden mitersetzt: Die Aufrufer geben Zustimmungszeilen,
/// Tabellenzellen und Listeneinträge aus, in denen eine zusätzliche Zeile eine gefälschte
/// Angabe wäre. Für bewusst mehrzeiligen Text gibt es [`sanitize_display_multiline`].
///
/// Harmloser Text bleibt Zeichen für Zeichen erhalten — Umlaute, Satzzeichen, Emoji und jede
/// andere Schrift. Eine zu scharfe Bereinigung wäre schlimmer als das Problem: Sie würde
/// deutsche Beschreibungen verstümmeln und den Text unlesbar machen.
///
/// ```
/// use sepp_core::sanitize_display;
/// assert_eq!(sanitize_display("Grüße — für Büroläufe 📎"), "Grüße — für Büroläufe 📎");
/// assert_eq!(sanitize_display("harmlos\r  gefälscht"), "harmlos\u{FFFD}  gefälscht");
/// ```
pub fn sanitize_display(s: &str) -> String {
    s.chars()
        .map(|c| {
            if is_unsafe_for_display(c) {
                DISPLAY_REPLACEMENT
            } else {
                c
            }
        })
        .collect()
}

/// Wie [`sanitize_display`], lässt aber echte Zeilenumbrüche durch.
///
/// Für die wenigen Stellen, an denen fremder Text bewusst über mehrere Zeilen geht. `\r` bleibt
/// verboten — allein steht er für „an den Zeilenanfang springen", und in der Kombination
/// `\r\n` genügt das `\n`.
pub fn sanitize_display_multiline(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\n' {
                c
            } else if is_unsafe_for_display(c) {
                DISPLAY_REPLACEMENT
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harmless_text_survives_untouched() {
        // Der wichtigste Fall: Eine zu scharfe Bereinigung wäre schlimmer als der Angriff.
        for ok in [
            "Eingangsrechnungen nach §14 UStG prüfen",
            "Grüße aus Köln — 100 % geprüft (v2.1)",
            "日本語のテキスト",
            "emoji 📎🔒 und Klammern [a] {b} <c>",
            "pfad/mit-bindestrich_und_unterstrich.txt",
            "",
        ] {
            assert_eq!(sanitize_display(ok), ok, "unverändert erwartet: {ok:?}");
        }
    }

    #[test]
    fn control_characters_become_visible() {
        let attack = "harmlos\r".to_string() + &" ".repeat(5) + "\u{1b}[32mGEPRÜFT\u{1b}[0m";
        let clean = sanitize_display(&attack);
        assert!(!clean.contains('\r'), "{clean:?}");
        assert!(!clean.contains('\u{1b}'), "{clean:?}");
        assert!(clean.contains(DISPLAY_REPLACEMENT));
        // Der lesbare Teil bleibt lesbar — der Mensch soll sehen, was da stand.
        assert!(clean.starts_with("harmlos\u{FFFD}"), "{clean:?}");
        assert!(clean.contains("GEPRÜFT"), "{clean:?}");
    }

    #[test]
    fn deceiving_characters_are_caught_too() {
        // Rechts-nach-links: `gro.esiob` erschiene als `boise.org`.
        let clean = sanitize_display("net \u{202E}gro.esiob");
        assert_eq!(clean, "net \u{FFFD}gro.esiob");
        // Breitenlos: zwei Namen, die gleich aussähen, bleiben unterscheidbar.
        assert_ne!(sanitize_display("de\u{200B}mo"), sanitize_display("demo"));
        assert_eq!(sanitize_display("\u{FEFF}demo"), "\u{FFFD}demo");
        assert_eq!(
            sanitize_display("a\u{2066}b\u{2069}c"),
            "a\u{FFFD}b\u{FFFD}c"
        );
    }

    #[test]
    fn c1_controls_are_not_a_backdoor() {
        // 0x9B ist CSI — dieselbe Wirkung wie ESC-[ , nur ein Byte kürzer.
        assert_eq!(sanitize_display("x\u{9b}31m"), "x\u{FFFD}31m");
    }

    #[test]
    fn every_replacement_keeps_the_character_count() {
        // Ein Zeichen für ein Zeichen: Spaltenbreiten in Tabellen bleiben berechenbar.
        let s = "a\r\nb\u{1b}c";
        assert_eq!(sanitize_display(s).chars().count(), s.chars().count());
    }

    #[test]
    fn the_check_and_the_cleanup_agree() {
        // Sonst hieße „sicher" an einer Stelle etwas anderes als an der nächsten.
        for s in [
            "harmlos",
            "mit\u{1b}Steuerzeichen",
            "mit\u{202E}Bidi",
            "mit\u{200B}Nullbreite",
            "Grüße 📎",
        ] {
            assert_eq!(
                is_display_safe(s),
                sanitize_display(s) == s,
                "uneinig über {s:?}"
            );
        }
    }

    #[test]
    fn multiline_variant_keeps_newlines_but_nothing_else() {
        let s = "Zeile 1\nZeile 2\r\u{1b}[2K";
        let clean = sanitize_display_multiline(s);
        assert!(clean.contains("Zeile 1\nZeile 2"), "{clean:?}");
        assert!(!clean.contains('\r'), "{clean:?}");
        assert!(!clean.contains('\u{1b}'), "{clean:?}");
        // Einzeilig ist auch der Umbruch weg.
        assert!(!sanitize_display(s).contains('\n'));
    }
}
