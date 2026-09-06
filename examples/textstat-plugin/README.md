# Ein WASM-Plugin für sepp mini schreiben

Dieses Verzeichnis ist ein vollständiges, lauffähiges Plugin und zugleich die Vorlage. Es ist mit
dem SDK `sepp-plugin` geschrieben: Der Autor schreibt **eine Funktion**, das Attribut
`#[sepp_plugin::tool]` erzeugt daraus das Aufrufprotokoll des Hosts. `src/lib.rs` ist deshalb
kurz — und alles darin ist Arbeit, kein Protokoll.

Das Beispiel zählt Zeichen, Wörter und Zeilen eines Textes und schätzt die Tokenzahl.

## In drei Schritten ausprobieren

```bash
just plugin-example                     # baut das Modul (installiert das Target bei Bedarf)
cp examples/textstat-plugin/target/wasm32-unknown-unknown/release/textstat.wasm ~/.sepp/plugins/
cp examples/textstat-plugin/textstat.toml ~/.sepp/plugins/
```

Beim nächsten Start meldet sepp `WASM: 1 Plugins geladen`, und `sepp policy` führt den Akteur
`plugin textstat` auf. Erweiterungen werden **nur beim Start** gelesen; nach jeder Änderung also
sepp neu starten.

Ein eigenes Plugin beginnt man nicht mit Kopieren, sondern mit `sepp plugin new <name>`: Das legt
`Cargo.toml`, `src/lib.rs`, Manifest und README als Gerüst an.

## So sieht ein Plugin aus

```rust
use sepp_plugin::prelude::*;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Der zu vermessende Text.
    text: String,
}

#[sepp_plugin::tool(desc = "Zählt Zeichen, Wörter und Zeilen.", label = "Textstatistik")]
fn textstat(args: Args, host: &Host) -> Result<ToolResult> {
    host.log("los");
    let words = args.text.split_whitespace().count();
    Ok(ToolResult::text(format!("{words} Wörter")).with_details(json!({ "words": words })))
}
```

- **`Args`** sind die Parameter des Werkzeugs. Das JSON-Schema, das das Modell sieht, entsteht
  daraus (`schemars`); Doc-Kommentare an den Feldern werden zu Beschreibungen. `serde` und
  `schemars` müssen direkte Dependencies deines Crates sein — ihre Derive-Makros verlangen das.
- **`#[sepp_plugin::tool]`** nimmt `desc` (Pflicht — das liest das Modell), optional `name`
  (Default: Funktionsname; muss `^[A-Za-z0-9_-]{1,64}$` erfüllen, sonst ein Compile-Fehler) und
  `label` (Anzeigename, Default: `name`). Genau ein `#[tool]` je Crate.
- **`Host`** ist der Zugang zum Host: `host.log(..)` immer; `host.fs()` nur mit dem Cargo-Feature
  `fs-read`; `host.http()` nur mit `net` (siehe unten).
- **Fehler** sind ein `Err(..)` — aus `String`, `&str`, JSON- und UTF-8-Fehlern per `?`. Das SDK
  macht daraus ein Ergebnis mit `is_error = true`. Ein Plugin trappt nie: Das Modell kann mit einer
  Erklärung etwas anfangen, mit einem abgestürzten Werkzeug nicht. Ungültige Argumente behandelt
  das SDK genauso.
- **`details`** geht an die Oberfläche, nicht ans Modell — der Platz für Zahlen, die man
  weiterverarbeiten will, ohne das Kontextfenster zu füllen. Was in `content` steht, kürzt der
  Host selbst (50 KiB / 2000 Zeilen).

Testen geht **ohne wasm32-Target**: Das Makro erzeugt ein Modul `__sepp_plugin_export` mit
`spec_json()` und `call_json(&[u8])`, das nativ läuft — `cargo test` im Plugin-Crate reicht.

## Wenn dein Plugin Rechte braucht

Drei Stellen müssen zusammenpassen, sonst lädt das Modul nicht:

```toml
# Cargo.toml — das Feature schaltet host.fs() UND den Host-Import frei
sepp-plugin = { …, features = ["fs-read"] }
```
```toml
# <name>.toml — das Manifest ist die Selbstauskunft des Autors
[capabilities]
fs_read = ["./daten"]
```
```toml
# ~/.sepp/policy.toml — die Gegenzeichnung des Nutzers (`sepp policy allow --global plugin.<name> fs_read ./daten`)
[plugin.textstat]
fs_read = ["./daten"]
```

Effektiv gilt der **Schnitt** aus Manifest und Policy. Durchgesetzt wird am Linker, nicht am
Manifest: Der Host registriert eine gegatete Funktion nur, wenn das Recht übrig bleibt. Ein Modul,
das sie importiert, ohne dass sie gewährt ist, lässt sich nicht instanziieren — die Startmeldung
nennt den fehlenden Abschnitt. Deshalb: Ein Feature nur setzen, wenn das Manifest das Recht
anfordert. Umgekehrt bewirkt ein Manifest, das Rechte fordert, ohne dass das Modul die Funktion
importiert, nichts. Prüfen lässt sich die Lage jederzeit mit `sepp policy`.

| Feature | Zugang | Host-Import | Gate im Manifest / in der Policy |
|---|---|---|---|
| — | `host.log(..)` | `host_log` | immer |
| `fs-read` | `host.fs().read(..)`, `read_to_string(..)` | `host_fs_read_bytes` (+ Abholweg `host_result_read`) | `fs_read` oder `fs_write` |
| `net` | `host.http().get(..).send()` | `host_http` (+ Abholweg) | `net` — **provisorisch**, der Host antwortet heute mit einem Fehler; Stufe 3 setzt es um |

`host.fs().read` liefert die Datei **roh** (`Vec<u8>`) — ein PDF kommt als PDF an. Der Pfad wird
wie bei den eingebauten Tools aufgelöst und gegen dieselbe Policy geprüft; ein Plugin kommt nicht
weiter als der Agent selbst. Höchstens 16 MiB.

## Drei Dinge, die man einmal falsch macht

**Es gibt keinen Zustand zwischen zwei Aufrufen.** Der Host verwirft die Instanz nach jedem
Werkzeugaufruf. Ein `static` ist beim nächsten Aufruf wieder leer; Sitzungen, Tokens oder Caches
müssen im Host leben (das ist Gegenstand von Stufe 3).

**Die Standardbibliothek trägt nur zur Hälfte.** Rust lässt sich für `wasm32-unknown-unknown` mit
`std` bauen, aber alles darin, was das Betriebssystem braucht, ist eine Attrappe: Eine Zeitmessung
schlägt fehl, `std::fs` gibt Fehler zurück, ein Zufallsgenerator hat keine Quelle. Ein Modul kann
nur, was der Host hineinreicht — Dateien also über `host.fs()`, nicht über `std::fs`.

**Das Wanduhr-Budget ist die reale Grenze.** wasmi ist ein Interpreter, grob 10- bis 20-mal
langsamer als nativ. Wer Dokumente verarbeitet, testet an einer zweiseitigen Rechnung, setzt
`max_wall_time_ms = 5000` — und der erste 90-seitige Sammelbeleg bricht ab. Limits großzügig
wählen und in der Anleitung nennen.

## Das Manifest

Pflicht ist einzig `name`, und zwar als Schlüssel: Unter diesem Namen sucht Sepp Guard den
`[plugin.<name>]`-Abschnitt. `kind` und `entry` sind heute reine Dokumentation und werden nicht
ausgewertet; der Loader geht über die vorgefundenen `*.wasm`-Dateien.

Wichtig ist `abi`: die Version des Protokolls, gegen die du gebaut hast. Fehlt die Angabe, gilt 1.
Ein höherer Wert als der, den dein sepp spricht, führt zur Ablehnung mit einer Meldung, die beide
Versionen nennt. Felder, die der Host nicht kennt, werden gelesen, ignoriert und beim Start
gemeldet — ein Tippfehler wie `capabilites` verschwindet also nicht stumm.

Gefunden wird das Manifest als `<stamm>.toml` neben `<stamm>.wasm`, ersatzweise als
`manifest.toml` im selben Verzeichnis, die dann für alle Module dort gilt. Deshalb setzt die
`Cargo.toml` hier `[lib] name = "textstat"`: Ohne das hieße das Artefakt `textstat_plugin.wasm`
und das Manifest müsste denselben Unterstrich tragen.

Der `[limits]`-Abschnitt deckelt den Verbrauch. Fehlt er, gelten 256 Seiten Speicher, dreißig
Sekunden Laufzeit und eine Million Instruktionen je Zeitscheibe. Rechenzeit läuft in Zeitscheiben:
Nach `fuel_slice` Instruktionen gibt das Modul die Kontrolle an den Host zurück, der prüft, ob
abgebrochen wurde oder die Uhr abgelaufen ist, und dann weiterlaufen lässt. Eine Endlosschleife
blockiert deshalb nichts, sie wird nur irgendwann beendet.

## Das Protokoll darunter (nur für Neugierige)

Wer ohne SDK baut — in einer anderen Sprache etwa — findet den Vertrag in `wit/sepp.wit` im
Repo-Root: die logische Schnittstelle als WIT und darunter die Kodierung für Core-WASM (ABI 1).
Kurzfassung: Das Modul exportiert `memory`, `sepp_alloc(i32)->i32`, `sepp_spec()->i64` und
`sepp_call(i32,i32)->i64`; `i64` packt `(ptr << 32) | len`; `sepp_spec` liefert ToolSpec-JSON,
`sepp_call` bekommt Argument-JSON und liefert ToolResult-JSON. Importe aus `env`: `host_log` und
`host_result_read` immer, `host_fs_read` und `host_fs_read_bytes` mit `fs_read`, `host_http` mit
`net` — fünf Funktionen, alle mit `(i32, i32)`-Parametern. Ein Test in `sepp-wasm` hält die
WIT-Datei und den Host synchron.

Grenzen des Hosts: höchstens 16 MiB je Rückgabe, 10 Millionen Instruktionen beim Instanziieren,
5 Sekunden Ladezeit inklusive `sepp_spec`.

## Prüfen, ohne sepp zu starten

```bash
cargo test -p sepp-wasm -- --ignored
```

Der Test baut dieses Beispiel, lädt es in den Host und ruft es auf. Er läuft nicht in der CI, weil
dort kein WASM-Target installiert ist.
