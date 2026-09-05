# Ein WASM-Plugin für sepp mini schreiben

Dieses Verzeichnis ist ein vollständiges, lauffähiges Plugin und zugleich die Referenz für das
Aufrufprotokoll. `src/lib.rs` ist in zwei Hälften geteilt: Der obere Teil ist bei jedem Plugin
gleich und darf kopiert werden, der untere ist die eigentliche Arbeit.

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

## Was der Host erwartet

Vier Exports, sonst nichts:

| Export | Signatur | Wann geprüft |
|---|---|---|
| `memory` | die exportierte Memory, Name exakt `memory` | beim Laden |
| `sepp_spec` | `() -> i64` | beim Laden |
| `sepp_alloc` | `(i32) -> i32` | **erst beim ersten Aufruf** |
| `sepp_call` | `(i32, i32) -> i64` | **erst beim ersten Aufruf** |

Der Rückgabewert `i64` trägt zwei Zahlen: im oberen Wort die Adresse, im unteren die Länge.

**`sepp_spec`** liefert die Werkzeugbeschreibung als JSON. Alle vier Felder sind Pflicht:

```json
{ "name": "…", "label": "…", "description": "…", "parameters": { "type": "object" } }
```

`parameters` ist ein JSON-Schema und geht unverändert an das Modell. Halte es schlank, ohne
`$schema` und ohne `title`.

**`sepp_call`** bekommt die Argumente als JSON und liefert das Ergebnis als JSON zurück. Pflicht
ist nur `content`; `details` und `is_error` darfst du weglassen:

```json
{ "content": [{ "type": "text", "text": "…" }], "is_error": false }
```

Was in `content` steht, sieht das Modell. Was in `details` steht, nicht: Das ist der Platz für
Zahlen, die die Oberfläche verarbeiten soll, ohne das Kontextfenster zu füllen.

## Vier Dinge, die man einmal falsch macht

**Das Vorzeichen beim Packen.** Ein `i32` mit gesetztem höchsten Bit schmiert beim Verbreitern
sein Vorzeichen in die oberen 32 Bit und zerstört die Länge. Deshalb die Zwischenstufe:

```rust
((ptr as u32 as i64) << 32) | (len as u32 as i64)
```

**Es gibt kein Freigeben.** Das Protokoll kennt keinen Gegenspieler zu `sepp_alloc`. Belegter
Speicher gehört ab dem Aufruf dem Host, und der Puffer muss die Rückkehr aus `sepp_call`
überleben, weil der Host ihn erst danach liest. Im Beispiel steht deshalb zweimal ein bewusstes
`std::mem::forget`. Das leckt nicht dauerhaft: Der Host wirft nach jedem Aufruf die ganze Instanz
weg. Aus demselben Grund kann ein Plugin **keinen Zustand über zwei Aufrufe hinweg** halten.

**Fehlende Exports fallen spät auf.** `sepp_alloc` und `sepp_call` sucht der Host erst beim ersten
Werkzeug-Aufruf. Ein Plugin ohne sie lädt scheinbar sauber und stirbt später mit
`wasm: sepp_alloc fehlt`. Wenn ein Plugin geladen wird, aber beim Benutzen umfällt: hier
nachsehen.

**Nur `host_log` importieren.** Die beiden anderen Host-Funktionen, `host_fs_read` und
`host_http`, registriert der Host nur, wenn die Policy das passende Recht gewährt. Importierst du
eine davon ohne Gewährung, scheitert schon die Instanziierung und das Plugin lädt gar nicht.
Beide sind derzeit ohnehin leere Hüllen und liefern konstant null; ein Plugin kann heute rechnen
und protokollieren.

## Wenn dein Plugin Rechte braucht

Das Manifest ist die Selbstauskunft des Autors, keine Grenze. Du deklarierst, was du willst:

```toml
[capabilities]
net = ["api.example.com"]
```

Wirksam wird es erst durch die Gegenzeichnung in der `policy.toml` des Nutzers:

```toml
[plugin.textstat]
net = ["api.example.com"]
```

Effektiv gilt der **Schnitt** aus beidem. Fehlt der Abschnitt, bekommt das Plugin nichts.

Durchgesetzt wird das am Linker, nicht am Manifest: Der Host registriert `host_http` nur, wenn das
Recht übrig bleibt. Importiert dein Modul die Funktion ohne die Gewährung, lässt es sich nicht
instanziieren und lädt gar nicht erst; die Startmeldung nennt dann den fehlenden Abschnitt.
Umgekehrt heißt das: Ein Manifest, das Rechte fordert, ohne die zugehörige Funktion zu
importieren, bewirkt nichts. Prüfen lässt sich die Lage jederzeit mit `sepp policy`.

## Das Manifest

Pflicht ist einzig `name`, und zwar als Schlüssel: Unter diesem Namen sucht Sepp Guard den
`[plugin.<name>]`-Abschnitt. `kind` und `entry` sind heute reine Dokumentation und werden nicht
ausgewertet; der Loader geht über die vorgefundenen `*.wasm`-Dateien.

Gefunden wird das Manifest als `<stamm>.toml` neben `<stamm>.wasm`, ersatzweise als
`manifest.toml` im selben Verzeichnis, die dann für alle Module dort gilt. Deshalb setzt die
`Cargo.toml` hier `[lib] name = "textstat"`: Ohne das hieße das Artefakt `textstat_plugin.wasm`
und das Manifest müsste denselben Unterstrich tragen.

Der `[limits]`-Abschnitt deckelt den Verbrauch. Fehlt er, gelten 256 Seiten Speicher, dreißig
Sekunden Laufzeit und eine Million Instruktionen je Zeitscheibe.

## Grenzen des Hosts

| | |
|---|---|
| Rückgabe je Aufruf | höchstens 16 MiB |
| Instruktionen beim Instanziieren | 10 Millionen |
| Ladezeit inklusive `sepp_spec` | 5 Sekunden |

Rechenzeit läuft in Zeitscheiben: Nach `fuel_slice` Instruktionen gibt das Modul die Kontrolle an
den Host zurück, der prüft, ob abgebrochen wurde oder die Uhr abgelaufen ist, und dann weiterlaufen
lässt. Eine Endlosschleife blockiert deshalb nichts, sie wird nur irgendwann beendet.

## Prüfen, ohne sepp zu starten

```bash
cargo test -p sepp-wasm -- --ignored
```

Der Test baut dieses Beispiel, lädt es in den Host und ruft es auf. Er läuft nicht in der CI, weil
dort kein WASM-Target installiert ist.
