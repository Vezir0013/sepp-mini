# Changelog

Alle nennenswerten Änderungen an diesem Projekt werden hier dokumentiert.

Das Format orientiert sich an [Keep a Changelog](https://keepachangelog.com/de/1.1.0/),
und das Projekt folgt [Semantic Versioning](https://semver.org/lang/de/).

## [Unreleased]

Vorarbeiten für Plugin-Pakete, aber unabhängig davon nützlich.

### Hinzugefügt
- **`host_fs_read_bytes` für WASM-Plugins.** `host_fs_read` liefert `from_utf8_lossy`; für ein
  PDF, ZIP oder Bild kommt dort Ersatzmüll an — es gab bisher überhaupt keinen Weg, binäre
  Dateien in ein Plugin zu bekommen. Die neue Fähigkeit legt die Datei **roh** ab und
  signalisiert über das Vorzeichen: `n >= 0` sind `n` Bytes Nutzdaten, `n < 0` sind `-n - 1`
  Bytes Fehlertext. Diese Abweichung von der JSON-Konvention ist Absicht: Base64 in einer Hülle
  zwänge das Modul, Kodierung und Ergebnis gleichzeitig zu halten — grob das 2,3-fache der
  Dateigröße gegen ein 16-MiB-Limit. Dasselbe Gate wie `host_fs_read` (`fs_read`, oder
  `fs_write`, das Lesen einschließt), und **additiv**: Ein Modul, das die Funktion nicht
  importiert, merkt nichts. Das ABI bleibt bei Version 1.

### Tests
- `host_fs_read_bytes` liefert rohe Bytes statt verlustbehaftetem Text (zwei ungültige
  UTF-8-Bytes: roh 2, lossy wären 6 — die Zahl unterscheidet beide Wege eindeutig); ein
  verweigerter Zugriff meldet sich negativ; die gemeinsame Prüfhälfte `read_granted_file` gegen
  Policy, Eingabefehler und byte-identische Rückgabe.

### Geplant
- Egress-Proxy für `net`-Hostfilter (Landlock/Seatbelt filtern nur Ports)
- `host_http` für WASM-Plugins (Signatur steht, Umsetzung folgt)
- Plugin-SDK, das Speicher und Zeiger kapselt — erst wenn Plugins ankommen
- Paketformat und `sepp pkg install`: mehrere Erweiterungsstufen gebündelt, Rechte als
  Zustimmung bei der Installation
- OpenTelemetry-Export (optional aktivierbar)
- OAuth-Login für Subscription-Provider
- Google-Provider-Adapter

## [0.2.0] - 2026-09-05

Eine Review über alle Crates hat fünfzehn Befunde ergeben, und sie fielen in Muster: Grenzen, die
erst hinter der Schranke greifen; Zusagen der Doku, die der Code nicht hält; Meldungen, die im
interessanten Fall schweigen. Kein einzelner davon war dramatisch — zusammen beschreiben sie
Stellen, an denen `sepp` etwas versprach, das er nicht hielt. Das ist bei einem Werkzeug, dessen
Alleinstellungsmerkmal die Rechteverwaltung ist, die teuerste Sorte Fehler.

**Warum die Minor-Version steigt und nicht nur die Patch-Nummer:** Eine Rechtezeile bedeutet ab
jetzt etwas anderes. Wer `[plugin.x] fs_write = ["./"]` schrieb und annahm, das Plugin könne
damit nicht *lesen*, lag schon vorher falsch — Landlock trägt jeden Schreibpfad zusätzlich in die
Leseliste ein, Seatbelt schreibt `(allow file-read* file-write* …)`, und `Policy::allows_path`
las es seit jeher großzügig. Nur `covers` widersprach und beschrieb damit einen Zustand, den es
im Betrieb nirgends gab. Diese Diskrepanz ist behoben — aber wer die strikte Lesart geglaubt hat,
sollte seine `policy.toml` einmal ansehen, statt es einem Patch-Release zu entnehmen. Alles
andere in diesem Release ist Reparatur oder Zuwachs.

### Hinzugefügt
- **Secrets für entfernte MCP-Server.** `[mcp.servers.headers]` nimmt HTTP-Header, deren Werte
  `$NAME`-Platzhalter enthalten dürfen; der Secret-Broker setzt sie **vor** dem Verbinden ein.
  Bisher gab es überhaupt keinen Weg, einem http-Server einen Schlüssel zu geben — und den
  Broker gab es zwar samt Tests, aber ohne einen einzigen Aufrufer. Durchgesetzt wird mit zwei
  Rechten aus der `policy.toml`: `net = ["<host>"]` sagt, wohin das Secret darf, `env = ["NAME"]`
  sagt, welches der Server sehen darf. Fehlt eine Hälfte, bricht der Connect mit dem passenden
  `sepp policy allow` ab, statt einen Header mit literalem `$NAME` loszuschicken — der landete
  sonst im Zugriffslog des fremden Servers. Substituiert wird nur in `headers`, nie in der `url`:
  Die steht in jeder Verbindungsfehlermeldung. Werte sind als `sensitive` markiert und tauchen
  in keinem `Debug` auf; Fehlertexte und das stderr von stdio-Servern laufen durch `redact`.
- **Unbekannte Schlüssel in der `policy.toml` werden gemeldet.** `[plugins.x]` statt `[plugin.x]`
  oder `fs_reed = [...]` parste bisher klaglos und bewirkte nichts. In der Datei, die über Rechte
  entscheidet, ist ein stumm verschluckter Tippfehler nicht kosmetisch: Man hält ein Recht für
  erteilt oder das Netz für gesperrt. Abgelehnt wird weiterhin nichts — sonst scheiterte jede
  neuere Policy auf einem älteren `sepp`.

### Geändert
- **Ein Schreibrecht deckt jetzt auch Leseanfragen.** Bisher hatten vier Stellen drei Meinungen:
  `Policy::allows_path` und beide Sandbox-Adapter zählten `fs_write` als Leserecht, `covers`
  (und damit `intersect`) trennte strikt, und das WASM-Linker-Gate prüfte nur auf `fs_read`.
  Ausschlaggebend ist die Durchsetzung: Landlock trägt jeden Schreibpfad zusätzlich in die
  Leseliste ein, Seatbelt schreibt `(allow file-read* file-write* …)`. „Schreiben ja, lesen nein"
  ist für Kindprozesse gar nicht ausdrückbar — die strikte Lesart beschrieb einen Zustand, den es
  im Betrieb nirgends gab. Die Gegenrichtung bleibt streng, und `[deny]` behält seine Asymmetrie.
- **`sepp policy` widerspricht dem Loader nicht mehr.** Ein Abschnitt mit nur `exec = "system"`
  wurde als „(kein Abschnitt) — keine Rechte" gemeldet, während der Loader ihn sehr wohl sah.
  Beide benutzen jetzt dieselbe Prädikatsfunktion. Für http-Server mit Secret-Headern nennt die
  Tabelle bei `net` und `env` den Broker als Vollstrecker — das ist der einzige Punkt, an dem
  ein remoter Server aufhört, rechtlich ein Nullum zu sein.
- **Ein kaputtes Plugin-Manifest ergibt eine Meldung statt zweier widersprüchlicher.** Die erste
  versprach einen Namens-Fallback auf den Dateistamm, den es nie gab: Das Manifest wurde gleich
  darauf ein zweites Mal geparst und riss das Plugin mit. Jetzt wird einmal geparst und das
  Plugin ehrlich übersprungen — ohne lesbares Manifest wäre auch `abi` unbekannt und würde
  stillschweigend als 1 gelesen.
- **Der Hinweis bei fehlenden Plugin-Rechten erklärt beide Hälften.** Er hing an der fehlenden
  Gewährung; wer der Empfehlung folgte und `sepp policy allow …` ausführte, bekam danach nur noch
  ein nacktes `unknown import`. Dabei ist genau das der Fall, in dem zu erklären war, dass das
  Manifest das Recht nicht anfordert.

### Behoben
- **`bash` puffert die Ausgabe eines Kindprozesses nicht mehr unbegrenzt.** `yes` oder
  `cat /dev/urandom` füllten bis zum Timeout den Speicher; die Trunkierung lief erst auf dem
  fertigen Puffer und konnte den Host konstruktionsbedingt nicht schützen. Jetzt greift eine
  Aufnahmegrenze je Strom, und verworfene Bytes werden im Ergebnis benannt.
- **Das Trunkierungsbudget gilt pro Tool-Ergebnis, nicht je Block.** Ein MCP-Server oder Plugin
  mit 200 Blöcken à 50 KiB brachte 10 MB ins Kontextfenster, weil jeder Block seine eigene
  Grenze bekam.
- **Die Obergrenze für WASM-Fähigkeiten gilt für das abgelegte Ergebnis.** `host_fs_read` prüfte
  die rohe Dateigröße und legte dann das JSON ab — Lossy-Ersetzungen und Escaping können das
  Vielfache sein, und dieser Speicher liegt beim Host, den das Page-Limit nicht deckt.
- **`read` mit `offset`/`limit` verfälscht keine Zeilenenden mehr.** Der Ausschnitt wurde über
  `lines().join("\n")` neu zusammengesetzt, was CRLF zu LF normalisierte und eine abschließende
  Newline verschluckte. Das Modell kopierte danach einen `old_string`, den es auf der Platte so
  nicht gab, und `edit` meldete „nicht gefunden" ohne erkennbaren Grund.
- **Die Auto-Compaction reagiert auf Ctrl+C.** Sie streamte mit einem frisch erzeugten
  Cancel-Token; der Nutzer saß den vollen Zusammenfassungs-Roundtrip ab. Gleiches galt für
  `/compact` in der TUI.
- **Ein fehlgeschlagener Sub-Agent-Lauf behält seinen Audit-Eintrag.** Die Kind-Session lag auf
  Platte, aber der Verweis in der Wurzel entstand erst nach der Fehlerbehandlung — verwaist war
  ausgerechnet der Fall, den man hinterher nachlesen will. Ein gescheiterter Auftrag ist jetzt
  ein Werkzeugfehler mit Spur; ein Abbruch reicht weiterhin durch.
- **`host_log` klemmt negative Zeiger.** Ein Plugin konnte mit `ptr = -1` einen
  Additions-Overflow auslösen — in Debug-Builds ein Panic aus dem Host-Call heraus, aus der
  Start-Sektion sogar außerhalb von `spawn_blocking`.
- **Das `lossy`-Flag von `host_fs_read` stimmt.** Es war ein Längenvergleich und meldete `false`,
  wenn die ersetzte Sequenz zufällig so lang war wie das Ersatzzeichen — etwa bei einer
  abgeschnittenen 4-Byte-Sequenz.
- **Die Pfad-Lock-Registry wächst nicht mehr unbegrenzt.** Sie legte je berührtem Pfad einen
  Eintrag an und entfernte nie einen — in einer langen TUI-Sitzung oder im RPC-Server monoton.

### Tests
- Aufnahmegrenze und Zählung verworfener Bytes in `bash`; gemeinsames Blockbudget; `line_slice`
  gegen CRLF, Trailing-Newline und Bereichsgrenzen; Aufräumen der Lock-Registry im Erfolgs- und
  im Fehlerfall.
- Compaction bricht auf ein gecanceltes Token ab (hängender Fake-Provider); gescheiterter
  Sub-Agent-Lauf trägt `details["audit"]` mit der Kind-Session.
- WASM: negativer `host_log`-Zeiger, gedeckeltes Ergebnis, `lossy` bei gleicher Länge, Plugin mit
  reiner `fs_write`-Gewährung lädt und liest, genau eine Meldung bei kaputtem Manifest.
- Policy: `fs_write` deckt `fs_read` (und nicht umgekehrt), Tippfehler je Abschnitt landen in den
  Warnungen, `[agent.ask]` bleibt dabei wirksam.
- MCP: die sieben Fehlerfälle von `resolve_headers` ohne Umgebung und ohne Server, `url_host`
  samt Userinfo/Port/IPv6, `Debug` eines Secret-Headers zeigt nur `Sensitive` — dazu ein
  Verdrahtungstest gegen einen echten TCP-Listener, der belegt, dass der Header auf der Leitung
  ankommt und dass ohne `net`-Gewährung gar keine Verbindung entsteht.

## [0.1.22] - 2026-09-05

Das Plugin-ABI ist **festgezurrt (Version 1)**. WASM soll die Plugin-Welt von sepp mini tragen,
bis hin zu einem Paketmanager mit Marktplatz. Das trägt nur, weil ein Modul von sich aus nichts
kann und die Installation eines fremden Pakets damit zu einer Zustimmung wird statt zu einer
Vertrauensfrage. Bevor das erste fremde Paket existiert, muss das Protokoll stehen: Danach ist
jede Änderung ein Bruch für alle.

### Geändert
- **BRUCH: `host_fs_read` und `host_http` haben neue Signaturen** (`(i32,i32) -> i32`). Eine
  Fähigkeit führt aus, legt ihr Ergebnis beim Host ab und meldet nur dessen Größe; abgeholt wird
  es mit dem neuen **`host_result_read(ptr, cap) -> i32`** in einen Puffer, den das Plugin selbst
  stellt. Die alte Form hätte verlangt, dass der Host aus der Host-Funktion heraus `sepp_alloc`
  aufruft — dieser Rücksprung läuft nicht resumierbar und kollidiert mit dem Fuel-Slicing, das
  den äußeren Aufruf in Zeitscheiben anhält. So wird nie doppelt gesendet, und niemand muss eine
  Puffergröße raten.
- **Alle vier Exports werden beim Laden geprüft**, Name und Signatur, direkt am kompilierten
  Modul ohne Store. Bisher fielen `sepp_alloc` und `sepp_call` erst beim ersten Werkzeug-Aufruf
  auf: Ein Plugin lud scheinbar sauber und starb später. Für ein Paket, das jemand installiert,
  muss „kaputt" beim Laden sichtbar sein.
- Eine Fähigkeit liefert **immer** ein JSON-Objekt, auch im Fehlerfall (`{"error":"…"}`), und
  trappt nie. `host_http` ist weiterhin eine Attrappe, aber eine ehrliche: Sie erklärt, statt
  eine Null zu liefern.
- Die Obergrenze von 16 MiB gilt jetzt in **beide** Richtungen; bisher nur Plugin zu Host.

### Hinzugefügt
- **`abi` im Plugin-Manifest** deklariert die Protokollversion; fehlend gilt als 1. Ein höherer
  Wert als der des Hosts wird abgelehnt, mit einer Meldung, die beide Versionen nennt. Ohne
  dieses Feld bräche jede spätere Protokolländerung stumm jedes vorhandene Plugin.
- **Unbekannte Manifest-Felder werden gemeldet**, statt still verschluckt zu werden. Ein
  Tippfehler wie `capabilites` kostete bisher eine lange Suche. Abgelehnt wird deshalb nicht:
  Das ließe jedes neuere Paket auf einem älteren sepp scheitern.
- **`host_fs_read` liest echte Dateien.** Der Pfad wird kanonisch aufgelöst und gegen dieselbe
  Policy geprüft, die auch `read`, `write` und `edit` benutzen; ein Plugin kommt also nicht
  weiter als der Agent selbst. Ergebnis ist `{"bytes":N,"text":"…","lossy":…}`.
- Die Anleitung nennt jetzt auch, dass die Standardbibliothek für `wasm32-unknown-unknown` nur
  zur Hälfte trägt: keine Uhr, kein Zufall, kein Dateizugriff außer über die Importe.

- **Ein Beispiel-Plugin mit Anleitung** unter `examples/textstat-plugin/`. Bis jetzt gab es im
  ganzen Repository kein Beispiel, kein SDK und keine Vorlage; das Aufrufprotokoll stand nur in
  Modul-Kommentaren, und die einzige Implementierung waren WAT-Schnipsel in den Tests. Damit war
  Tier 2 ein Sicherheitsversprechen, das niemand einlösen konnte. Das Beispiel zählt Zeichen,
  Wörter und Zeilen, zeigt den Protokollteil offen zum Kopieren und benennt die vier Fallstricke:
  das Vorzeichen beim Packen von Adresse und Länge, das fehlende Freigeben, die Host-Funktionen,
  die man ohne Gewährung nicht importieren darf, und die nur halb tragende Standardbibliothek.
- **`just plugin-example`** baut das Beispiel und installiert das WASM-Target bei Bedarf.
- Ein `#[ignore]`-Test baut das Beispiel, lädt es in den Host und ruft es auf
  (`cargo test -p sepp-wasm -- --ignored`). Er läuft nicht in der CI, weil dort kein WASM-Target
  installiert ist, hält aber fest, dass Beispiel und Protokoll zusammenpassen.
- Anleitung zum Plugin-Bau in README und Handbuch; bisher erwähnte beides das ABI mit keinem Wort.

### Behoben
- `.gitignore` erfasste weder das `target/` eines Unterprojekts (der Eintrag ist an der Wurzel
  verankert) noch gebaute `*.wasm`-Dateien.

## [0.1.21] - 2026-09-05

Sepp Guard: **ein Regelwerk, eine Datei.** Der Leitsatz stimmte beim Regelwerk nicht. Rechte für
MCP-Server standen in zwei Dateien mit unterschiedlicher Verknüpfung, ein Verbot konnte kein Netz
zurücknehmen, und ein WASM-Plugin bekam ohne Gegenzeichnung, was sein eigenes Manifest verlangte.
Jetzt sagt die `settings.toml`, **was läuft**, und die `policy.toml`, **was es darf**.

### Geändert
- **BRUCH: `[mcp.servers.capabilities]` in der settings.toml wird nicht mehr ausgewertet.**
  Rechte eines MCP-Servers kommen ausschließlich aus `policy.toml [mcp.<name>]`; die frühere
  Vereinigung beider Quellen entfällt. Steht der alte Block noch da, meldet sepp das beim Start
  und `sepp policy` zeigt ihn als wirkungslos an. Es gibt bewusst keinen Migrationsbefehl.
- **BRUCH: Ein WASM-Plugin ohne `[plugin.<name>]` bekommt keine Rechte.** Bisher galt in diesem
  Fall das Manifest allein. Ein Manifest liegt aber neben der wasm-Datei und stammt vom Autor des
  Plugins; ohne Gegenzeichnung ist es eine Absichtserklärung, keine Grenze. Durchgesetzt wird am
  Linker: Ein Modul, das eine gegatete Host-Funktion importiert, lädt ohne die Gewährung nicht.
- `WasmHost::discover_with` liefert zusätzlich die Meldungen zu übersprungenen Plugins;
  `WasmHost::discover` und `load_file` sind entfallen. `sepp_mcp::connect` verbindet ohne Rechte
  und ist nur noch für `examples/probe.rs` gedacht.

### Hinzugefügt
- **`[deny] net`** nimmt Netzrechte zurück, gegen jede Quelle und jeden Akteur. `net = true` oder
  `net = ["*"]` ist der Hauptschalter. Eine konkrete Hostliste sperrt ebenfalls alles und sagt
  warum: Hostfilter brauchen den Egress-Proxy. Ein Verbot, das nicht wirkt, wäre gefährlicher als
  eines, das zu breit wirkt. Für `exec` und `env` bleibt es bei einer Warnung, weil Landlock dort
  nur Erlaubnislisten kennt.
- Unter einem Netzverbot wird ein MCP-Server mit `transport = "http"` **gar nicht erst
  verbunden**. Er wäre sonst der einzige Weg, doch nach draußen zu kommen.

### Behoben
- **Übersprungene Plugins und MCP-Server waren in der TUI unsichtbar.** Sie wurden nur geloggt
  bzw. auf stderr geschrieben, und die TUI zeigt beides nicht. Jetzt erscheinen sie als
  Startmeldung, bei fehlender Gewährung samt Namen des fehlenden Abschnitts.
- Ein unlesbares Plugin-Manifest fiel beim Namen still auf den Dateistamm zurück. Damit wurden
  die Rechte unter dem falschen Namen gesucht. Das wird jetzt gemeldet.
- Warnungen zum Regelwerk erschienen im Modus `yolo` nie, weil sie im falschen Zweig hingen.
- `/policy` in der TUI zeigte weder MCP-Server noch Plugins, weil die Zeilen dort nicht
  eingesammelt wurden. Terminal und TUI zeigen jetzt dasselbe.

## [0.1.20] - 2026-09-05

Sepp Guard, Phase 3: **die Spur.** Phase 1 hat den Agenten eingesperrt, Phase 2 hat ihn fragen
lassen — aber wer hinterher wissen wollte, was passiert ist, musste Fließtext im Modellkontext
lesen und fand von Sub-Agenten gar nichts. Jetzt ist jede Entscheidung ein eigener Eintrag, jede
Delegation eine eigene Sitzung, und `sepp audit` liest beides vor.

### Hinzugefügt
- **`sepp audit [<id>]`** gibt die Spur einer Sitzung lesbar aus: Prompts, Antworten, Tool-Aufrufe
  samt Ergebnis, Guard-Entscheidungen mit Grund und delegierte Sub-Agenten. Die Kind-Session eines
  Sub-Agenten wird eingerückt aufgeklappt. Ohne Argument die jüngste Sitzung des Projekts, sonst
  ein ID-Präfix. `--no-children` lässt die Kind-Sessions zu, `--json` gibt ein Objekt je Eintrag
  aus (`sepp audit --json | jq 'select(.entry.payload.kind == "guard")'`).
- **Guard-Entscheidungen stehen in der Session**, als Einträge der Art `guard` — auch die
  erlaubten, sonst zeigt die Spur nur Ausnahmen und nie den Normalfall. Auch Verweigerungen sind
  erfasst, die als Fehler aus dem Tool kommen und deshalb kein Ergebnis mit Details haben.
- **Sub-Agenten schreiben eine eigene Kind-Session**, die im Header über `parent_session` auf ihre
  Wurzel verweist; die Wurzel bekommt einen Eintrag der Art `subagent` mit ID, Aufgabe und Umfang.
  Der Verlauf des Sub-Agenten bläht den Wurzel-Kontext weiterhin nicht auf — er ist jetzt nur
  nicht mehr verloren. Auch ein abgebrochener Lauf wird geschrieben.
- **Zwei Einhängepunkte in `sepp-agent`**, damit die Crate policy-frei bleibt: eine `AuditSource`,
  die der Loop nach jedem Tool-Batch abfragt, und der reservierte Schlüssel `details["audit"]`
  eines `ToolResult`, den der Loop als eigenen Eintrag schreibt.

### Geändert
- **Session-Dateien werden mit `0600` in einem `0700`-Verzeichnis angelegt.** Sie enthalten alles,
  was der Agent gelesen und geschrieben hat; bisher galt die umask.
- `/tree` blendet Guard-Einträge aus (sie stünden sonst zwischen jedem Tool-Aufruf) und zeigt eine
  Delegation als `→ Sub-Agent <id>`.
- Ein mehrdeutiges Session-Präfix bei `-r`/`sepp audit` ist jetzt ein Fehler mit Vorschlägen,
  statt still die zuletzt geänderte Sitzung zu nehmen.

### Behoben
- **Der Audit-Eintrag in `details["guard"]` konnte vom falschen Tool stammen.** Er wurde nach der
  Autorisierung aus dem gemeinsamen Guard-Protokoll gefischt (`last_audit`); da Tool-Aufrufe
  parallel laufen, konnte dort die Entscheidung eines anderen Aufrufs stehen. Die Entscheidung
  reist jetzt in der `Authorization` des Aufrufs mit.
- Das Guard-Protokoll wuchs über die gesamte Sitzung, weil es nie abgeholt wurde; der neue
  Loop-Haken leert es nach jedem Tool-Batch.

## [0.1.19] - 2026-09-05

Sepp Guard, Phase 2: **der Modus `ask` fragt jetzt wirklich.** Phase 1 hat den Agenten eingesperrt,
aber außerhalb der Policy nur verweigert — jede Ausnahme kostete einen Neustart mit angepasster
Datei. Jetzt erscheint in der TUI ein Dialog: einmal, für die Sitzung, dauerhaft oder nein. Die
Antwort „dauerhaft" schreibt das Recht selbst in die `policy.toml`, ohne Kommentare zu zerstören.

### Hinzugefügt
- **Rückfrage-Dialog in der TUI** (Modus `ask`): zeigt Akteur, Aktion und Grund; Antworten per
  Direkttaste `e`/`s`/`d`/`n` oder ↑/↓ und Enter, Esc lehnt ab. Der laufende Turn bleibt aktiv,
  während das fragende Tool wartet; parallele Tool-Aufrufe reihen sich in eine Warteschlange
  (Anzahl offener Fragen steht im Rahmen). Turn-Ende und Ctrl+C lehnen offene Fragen ab —
  es wird nie stillschweigend erlaubt.
- **`sepp policy allow [--global] <akteur> <recht> <wert>`** trägt das Recht selbst ein
  (projektlokal `.sepp/policy.toml`, mit `--global` in `<config_root>/policy.toml`). Neu ist
  `sepp_policy::policy_edit` auf Basis von `toml_edit`: Kommentare, Reihenfolge und Formatierung
  bleiben erhalten, ein vorhandener Wert ist ein No-op, fehlende Abschnitte werden angelegt.
- **`/policy` in der TUI** zeigt dasselbe Regelwerk wie `sepp policy` unter dem Verlauf.
- `Guard::set_prompter` (der Rückfrage-Kanal entsteht erst mit der TUI, der Guard steckt da schon
  in den Tools), `Guard::take_notices` für Frontend-Meldungen, Sitzungs-Zustimmung für
  Shell-Kommandos (ein per Muster bestätigtes Kommando fragt nicht erneut).

### Geändert
- `ask` ohne Terminal (`-p`/`--rpc`) fällt mit Startup-Hinweis auf `auto` zurück, statt jede
  Aktion außerhalb der Policy zu verweigern — dort gibt es niemanden zu fragen.
- Die Verweigerungsmeldung nennt nicht mehr „Nachfrage-Dialog folgt (Phase 2)".

### Tests
- Dialog als Reducer getestet: Direkttasten, ↑/↓ mit Klemmung, Enter, Esc, unbekannte Taste,
  Warteschlange mit zwei Anfragen, Ablehnung bei Turn-Ende und Ctrl+C, `/policy` ohne Guard.
- `policy_edit`: Kommentare bleiben, No-op bei vorhandenem Wert, neue Datei mit Kopf, `[mcp.git]`
  als eine Zeile, `net = true` als Bool, klare Fehler bei `exec = "system"` und unbekanntem Recht.
- `parse_allow_args` (inkl. `--global` an beliebiger Stelle) und `section_label`.
- End-to-End in der echten TUI über ein Pseudo-Terminal gegen Ollama: alle vier Antworten
  (`n` verweigert, `e`/`s` schreiben ohne Datei-Eintrag, `d` schreibt und trägt ein).

## [0.1.18] - 2026-09-05

Sepp Guard, Phase 1: **ein Regelwerk, ein Entscheider, ein Audit, mehrere Vollstrecker.** Bisher
waren Erweiterungen eingesperrt, der Agent selbst nicht — `bash`, `read`, `write` und `edit` liefen
mit vollen Nutzerrechten, ohne Pfadgrenze und ohne Bestätigung; das bash-Tool entfernte nur vier
Provider-Keys. Jetzt läuft `bash` in derselben OS-Sandbox wie MCP-Server (Environment-Allowlist,
Landlock/Seatbelt, TCP-Verbot ohne `net`), `read`/`write`/`edit` prüfen Pfade gegen dieselbe Policy,
und `sepp policy` zeigt, wer was darf und wer es durchsetzt.

### Hinzugefügt
- **Sepp Guard** (`sepp-policy::guard`): Policy-Datei `.sepp/policy.toml` (projektlokal, nach
  Trust), `~/.sepp/policy.toml` und `[policy]` in `settings.toml`; Abschnitte `[agent]`,
  `[agent.ask]`, `[mcp.<name>]`, `[plugin.<name>]`, `[deny]`. Vereinigung aller Gewährungen mit
  Herkunft, `[deny]` gewinnt immer (`fs_read` sperrt Lesen+Schreiben, `fs_write` nur Schreiben).
  Eingebaute Defaults gelten immer: Projekt + Systempfade lesbar, Projekt + `$TMPDIR` schreibbar,
  Ausführen unbeschränkt, kein Netz, minimale Umgebung, Verbote auf `~/.ssh ~/.aws ~/.gnupg ~/.sepp`
  plus config-/state-Root. Entscheider `Guard::decide`/`authorize` mit Modus-Tabelle
  `ask | auto | yolo`, Audit je Entscheidung, `PermissionPrompter`-Trait für den Dialog (Phase 2).
- **`--mode ask|auto|yolo`** und `SEPP_MODE`; Default `ask` in der TUI, `auto` bei `-p`/`--rpc`.
  `yolo` schaltet den Guard ab (bisheriges Verhalten, mit Hinweis). `ask` verhält sich in Phase 1
  wie `auto` und sagt beim Start, dass der Dialog folgt.
- **`sepp policy`**: effektive Rechte je Akteur (Agent, MCP-Server, Plugins) als Tabelle mit
  Quelle und Vollstrecker, Verbote, Rückfrage-Muster und die Zeile „Nicht durchsetzbar auf diesem
  System". `sepp policy allow <akteur> <recht> <wert>` nennt vorerst Datei und TOML-Schnipsel.
- **Startprobe fail-closed**: kann die Sandbox nicht durchgesetzt werden (Kernel ohne Landlock,
  `sandbox_init`-Fehler), startet der Agent nicht; Ausweg ist explizit `--mode yolo`. Fehlt nur
  das TCP-Verbot (Kernel < 6.7), gibt es einen Start-Hinweis. Deny-Überlappungen (Verbot unter
  Gewährung) werden gemeldet.
- **Landlock ABI v7** statt v1 (BestEffort): Truncate, Refer, IoctlDev werden mitgehandhabt;
  **TCP-Verbot** ohne `net`-Recht (Kernel ≥ 6.7); **Exec-Allowlist** bei `exec`-Liste (Execute nur
  auf die Programme, das gestartete Programm und die dynamischen Loader). Seatbelt: `(allow
  network*)` nur mit `net`, `process-exec (literal …)` bei Exec-Liste, Deny-Zeilen für `[deny]`.
  `kernel_capabilities()` fragt ab, was der Kernel kann, ohne den eigenen Prozess zu beschränken.
- `sepp init` schreibt eine kommentierte `policy.toml` und aktiviert das Preset für erkannte
  Projekttypen (`Cargo.toml` → Rust, `package.json` → Node, `pyproject.toml`/`requirements.txt`
  → Python). Ohne `sepp init` gelten die Minimal-Defaults.
- `sepp-tools`: `builtin_tools_with(guard)`; die Tools tragen den Guard,
  `ToolResult.details["guard"]` enthält den Audit-Eintrag; ein an der Sandbox gescheitertes
  bash-Kommando bekommt einen `[guard: …]`-Hinweis für das Modell.
- `sepp-mcp`: `connect_with_policy` (gemergte Policy vom Frontend) und `policy_from_config`; stderr
  des Servers wird gepipet und über `tracing` geloggt statt in die TUI geerbt.
- `sepp-wasm`: `load_file_with_grant` / `discover_with` — effektiv gilt der Schnitt aus
  Manifest-Anfrage und `[plugin.<name>]`-Gewährung; ohne Gewährung wie bisher das Manifest.
- `sepp-policy`: `Policy::union/intersect/without_denied/allows_path`, Wildcard-Host `*`,
  `ResolveCtx`/`resolve_path_with` (testbar ohne Env), `canonicalize_lenient`
  (Symlink-sicher auch für neue Dateien), `probe_sandbox`, `resolve_program`.

### Geändert
- Unter Guard ist das Environment des bash-Tools Default-deny (Allowlist + `[agent].env`), nicht
  mehr nur eine Blacklist; die Shell sieht z. B. `CARGO_HOME` nur noch, wenn es freigegeben ist.
- MCP-Kindprozesse erben stderr nicht mehr.
- Neue Crate-Kanten: `sepp-tools → sepp-policy`, `sepp-cli → sepp-policy` (bleibt azyklisch).

### Behoben
- bash-Tool: `ZAI_API_KEY` und `MOONSHOT_API_KEY` fehlten in der Key-Blacklist — die Keys waren
  per Prompt-Injection über die Shell auslesbar.
- `SeppError::Io`/`Serde` nannten die Ursache nicht („io error") — jetzt mit Quelle, z. B.
  `io error: Permission denied (os error 13)`; aufgefallen an einem root-eigenen `~/.sepp/sessions`.
- Der `[guard: …]`-Hinweis des bash-Tools erschien nur bei englischen Fehlermeldungen und nur bei
  Exit-Code ≠ 0; jetzt auch bei deutschen (`Keine Berechtigung`) und unabhängig vom Exit-Code.
- Rhai `print()`/`debug()` schrieben auf stdout und konnten den RPC-/One-shot-Datenkanal stören;
  jetzt nach `tracing`.
- Landlock v1 beschränkte `truncate(2)` außerhalb erlaubter Pfade nicht (Truncate-Recht kam mit v3).

### Tests
- Policy: Parser, Merge mit Herkunft, Deny-Präzedenz, Wildcard-Host, Schnitt, Pfadauflösung ohne
  Env-Mutation, Modus-Tabelle, Fake-Prompter (Once/Session/Always/No), Audit.
- Sandbox (`#[ignore]`, echter Linux-Host): TCP-Deny/Allow via `/dev/tcp`, Exec-Allowlist,
  Schreibsperre; Seatbelt-Profil pur (Netz, Exec-Literale, Deny-Zeilen); `resolve_program`.
- Tools: read/write/edit innerhalb/außerhalb, Symlink-Escape, Env-Scrubbing unter Guard; bash
  unter Landlock (`#[ignore]`).
- CLI: `--mode`, `policy`-Unterbefehl, Modus-Präzedenz, Tabellen-Renderer, Template, Preset-Erkennung,
  `init` idempotent; `hooks`: `print()` im Hook; `mcp`: Legacy-Policy; `wasm`: Gewährungs-Schnitt.

## [0.1.17] - 2026-07-26

Moonshot AI (Kimi) kommt als sechster Provider dazu — und bringt zwei Annahmen ins Wanken, die
bisher für jeden OpenAI-kompatiblen Endpunkt galten. Erstens heißt das Output-Budget dort
`max_completion_tokens`, weil Moonshot `max_tokens` als deprecated führt und sein Rate-Limit gegen
das neue Feld rechnet. Zweitens ist Reasoning bei Kimi **nicht abschaltbar**: die API kennt nur
`low`/`high`/`max` und keinen Aus-Zustand, weshalb `--no-think` hier die Stufe senkt statt das
Denken zu beenden — sichtbar gemacht durch Hinweise beim Start und im TUI. Weil das Denken gegen
dasselbe Budget zählt und Kimi K3 ein 1M-Kontextfenster mitbringt, ziehen zwei Defaults mit: ein
größeres Output-Budget für Moonshot-Modelle und eine absolute Obergrenze für die
Auto-Compaction-Schwelle.

### Hinzugefügt
- **Moonshot AI / Kimi als Provider** (`--provider moonshot` bzw. `SEPP_PROVIDER=moonshot`).
  Dedizierter Connector (`crates/sepp-provider/src/moonshot.rs`, Feature `moonshot = ["openai"]`)
  gegen den OpenAI-kompatiblen Endpunkt `https://api.moonshot.ai/v1` — kein neuer Parser, der
  SSE-Decoder wird geteilt. `name()` liefert `"moonshot"`, alle Fehler-/Stream-Texte tragen
  `moonshot:` statt `openai:`. Key aus `MOONSHOT_API_KEY` (Pflicht), Endpunkt über
  `MOONSHOT_BASE_URL` überschreibbar (z. B. China-Region `https://api.moonshot.cn/v1`). Fehlt der
  Key, scheitert der Start früh mit einem hilfreichen Hinweis statt mit einem rohen 401.
- **Modell `kimi-k3`** in der Registry (1.048.576 Token Kontext). Damit leitet `sepp -m kimi-k3`
  den Provider automatisch ab, ohne `--provider`. Default-Modell für `--provider moonshot`.
- **Zwei Moonshot-Besonderheiten im Request-Body** (`OpenAiDialect::Moonshot`): das Output-Budget
  geht als `max_completion_tokens` raus (`max_tokens` ist bei Moonshot deprecated, und das
  Rate-Limit-Accounting hängt am neuen Feld), und `reasoning_effort` wird als Kostenregler
  gesendet — `low`/`high`/`max` statt eines An/Aus-Schalters.

### Geändert
- **`--no-think` bedeutet bei Moonshot „billig denken", nicht „aus".** Kimi kann Reasoning nicht
  abschalten (die API kennt kein `"none"`), deshalb sendet `Off` die Stufe `low` statt das Feld
  wegzulassen — ein weggelassenes Feld hieße Moonshots Default `max`, also das Gegenteil. Start
  und TUI-`/think` weisen darauf hin. Reasoning ist bei Moonshot per Default an (wie bei z.ai).
- **`--max-tokens`-Default ist bei Moonshot-Reasoning-Modellen 32768 statt 8192.** Das nicht
  abschaltbare Denken zählt gegen dasselbe Output-Budget; 8192 hätte die Antwort abgeschnitten
  (`finish_reason: "length"`). Nie über `max_output_tokens` des Modells hinaus; ein explizites
  `--max-tokens` bleibt unangetastet. Für alle anderen Provider ändert sich nichts.
- **`custom_model` ist jetzt auch beim Output-Budget provider-bewusst**, nicht nur beim
  Kontextfenster: unregistrierte Moonshot-IDs (`kimi-k2.7-code`, `kimi-k2.6`, …) erben 256k
  Kontext und dasselbe 32768er-Budget wie `kimi-k3`. Ohne das hätte der pauschale 8192er-Wert den
  neuen Default sofort wieder heruntergedeckelt — das größere Budget hätte nur für das eine
  registrierte Modell gegolten, und alle anderen Kimi-Modelle wären still in abgeschnittene
  Antworten gelaufen.
- **Auto-Compaction-Schwelle hat jetzt eine absolute Obergrenze** (`MAX_COMPACT_THRESHOLD` =
  256_000 in `sepp-agent`). 3/4 eines 1M-Fensters wären 786.432 Token, bevor überhaupt
  komprimiert wird — jeder Turn sendet den vollen Kontext erneut. Für alle bisherigen Modelle
  (größtes Fenster 200k) ist die Änderung ein No-op.

### Tests
- **Moonshot-SSE-Fixture** (`crates/sepp-provider/tests/fixtures/moonshot_basic.sse`) plus
  Decoder-Test inklusive Ordering-Invariante. Ein zweiter Test deckt einen Kimi-Stream **ohne**
  `reasoning_content` ab: `kimi-k3` streamt das Feld (live bestätigt), das ChoiceDelta-Schema der
  API-Referenz listet es aber nicht — bei anderen Modellen kann es also fehlen.
- **`moonshot_no_retry.rs`**: belegt gegen einen Mini-HTTP-Server, dass pro Turn genau ein
  Request rausgeht (der Connector erbt den 4xx-`reasoning_effort`-Retry des OpenAI-Adapters
  bewusst nicht — auf einer Bezahl-API träfe der auch 401 und 429) und dass
  `max_completion_tokens` statt `max_tokens` gesendet wird.
- **Moonshot Live-Smoke-Test** (`crates/sepp-provider/tests/moonshot_live.rs`). Per Default
  `#[ignore]`; läuft nur über `just test-live` mit gesetztem `MOONSHOT_API_KEY`.
- **`custom_model` ist jetzt getestet** (`custom_model_is_provider_aware`): Kontextfenster und
  Output-Budget je Provider. Der Moonshot-Fall im `default_max_tokens`-Test prüft nun eine
  unregistrierte ID gegen 32768 statt gegen 8192 — der alte Wert war in beiden Zweigen derselbe
  und hätte einen nie greifenden Moonshot-Zweig nicht bemerkt.

### Behoben
- **Leeres `SEPP_PROVIDER` bricht den Start nicht mehr.** `SEPP_PROVIDER=` (aus Shell-Profil oder
  CI) ergab `Some("")` und landete im „unbekannter Provider: "-Fehler, statt auf die Ableitung
  aus `-m` bzw. den Anthropic-Default zurückzufallen. Die Variable wird jetzt wie alle anderen
  Env-Werte getrimmt (leer/Whitespace = nicht gesetzt) — dieselbe Korrektur, die `OPENAI_BASE_URL`
  in 0.1.12 bekommen hat.
- **`sepp --help` listet `/think`** in der TUI-Befehlszeile. Der Befehl existiert seit 0.1.16, war
  in der Hilfe aber nicht aufgeführt — bei Moonshot ist er die einzige Laufzeit-Stellschraube für
  die Reasoning-Kosten.

## [0.1.16] - 2026-07-13

Extended Thinking wird vom Anzeige-Feature zum korrekt verdrahteten Rundlauf: Bisher (Phase 1)
wurden Thinking-Blöcke nach dem Streamen weggeworfen — das brach Anthropic-Thinking bei
Tool-Use (400 im Folge-Request) und ließ Ollama-Antworten teils leer enden, weil das Denken
unkontrolliert ins `reasoning`-Feld lief. Jetzt ist Thinking über die Provider konsistent:
signiert zurückgesendet, lokal steuerbar und in der TUI zur Laufzeit umschaltbar.

### Behoben
- **Anthropic: Extended Thinking + Tool-Use lehnte den Folge-Request mit 400 ab**
  (`sepp-provider`, `sepp-agent`): Die API verlangt den unveränderten Thinking-Block inkl.
  `signature` im nächsten Request. Der SSE-Mapper erfasst `signature_delta` als neues
  `StreamEvent::ThinkingSignature`, der Agent-Loop schließt den Thinking-Buffer je Signatur
  zu signierten Blöcken ab, und `block_to_json` sendet sie im Wire-Format (Feld `thinking`,
  nicht `text`) zurück. Unsignierte Blöcke (Fremd-Provider-Reasoning, Alt-Sessions) werden
  weiterhin weggelassen — die API lehnt sie ab. Zurückgesendet wird nur in Requests, die
  Thinking auch aktivieren: Bei deaktiviertem Thinking verbietet die API die Blöcke ebenso
  (400 „must have thinking enabled") — ohne diesen Drop bräche nach einem Thinking-Turn
  jede Compaction (summarisiert immer ohne Thinking), `/think off` und `--resume` ohne
  `--think`.
- **Ollama (`--provider local`): finale Antwort landete teils komplett im `reasoning`-Feld**,
  stdout blieb leer (`sepp-provider`): Der neue `OpenAiDialect::Local` sendet bei
  `ThinkingLevel::Off` `reasoning_effort: "none"` und steuert damit Ollamas
  Server-Default-Thinking; der SSE-Mapper bildet Ollamas `reasoning`-Delta als
  `ThinkingDelta` ab. Der Startup-Hinweis „--think/SEPP_THINK hat keine Wirkung" gilt nur
  noch für `openai`/`mlx` — bei `local` wirkt der Schalter jetzt. Endpunkte, die das Feld
  bzw. den Wert `"none"` nicht kennen (Ollama < 0.18: „invalid think value"; vLLM je nach
  Modell), lehnen mit 4xx ab — der Provider wiederholt den Request dann einmal ohne das
  Feld und lässt es für den Rest der Sitzung weg, statt dass `--provider local` gegen
  solche Server komplett bricht.

### Hinzugefügt
- **`/think` in der TUI** (`sepp-cli`, `sepp-agent`): schaltet Reasoning zur Laufzeit —
  ohne Argument als Toggle (Off ↔ Medium), mit `on`/`off` explizit; Medium wie `--think`
  (Anthropic verlangt `budget_tokens < max_tokens`). Neues `AgentSession::set_thinking`;
  der Loop liest `state.thinking` pro Request. Bei aktivem Reasoning zeigt die Status-Bar
  ein dezentes `think`-Segment (aus dem MetricCache — der Render-Pfad lockt die Session
  nie). Bei `openai`/`mlx` kommt der „keine Wirkung"-Hinweis auch hier; Sub-Agenten
  (`task`) frieren die Stufe beim Start ein, wie bei `/model`.

## [0.1.15] - 2026-07-12

Ressourcen-Limits und kooperatives Scheduling für WASM-Plugins: Das Sicherheitsmodell deckte
bisher *Zugriff* ab (Capabilities → Host-Funktionen), aber nicht *Verbrauch* — eine
Endlosschleife fror den Tool-Dispatch ein, eine `memory.grow`-Schleife fraß Host-RAM. Diese
Lücke ist geschlossen: Ein Plugin kann den Agenten weder aufhängen noch fluten.

### Hinzugefügt
- **`[limits]`-Abschnitt im Plugin-Manifest** (`sepp-policy`): `max_memory_pages` (Default 256
  = 16 MiB), `max_wall_time_ms` (Default 30 000; `0` = unbegrenzt lange laufen dürfen, aber
  weiterhin unterbrechbar) und `fuel_slice` (Default 1 000 000 Instruktionen pro Zeitscheibe).
  Fehlender Abschnitt heißt konservative Defaults, nicht „unbegrenzt"; unplausible Werte
  (`fuel_slice = 0`, `max_memory_pages` außerhalb `1..=65536`) lehnen das Manifest ab.
- **Fuel-Slicing mit Refuel-Loop** (`sepp-wasm`): Die Engine läuft mit Fuel-Metering; jeder
  Plugin-Export wird über wasmis Resumable-API (`call_resumable`/`resume`) in Zeitscheiben
  ausgeführt. Bei leerem Tank kommt die Kontrolle zum Host zurück (Yield-Punkt), der Abbruch
  und Wanduhr prüft, nachtankt (mindestens `required_fuel`, sonst käme eine Operation, die
  mehr als eine Scheibe kostet, nie voran) und die Ausführung **im erhaltenen Zustand**
  fortsetzt — kein Neustart, lange legitime Rechnungen laufen korrekt zu Ende.
- **Mid-Call-Abbruch:** Das `CancellationToken` wandert in den `spawn_blocking`-Lauf und wird
  an jedem Yield-Punkt geprüft — Ctrl-C bricht ein rechnendes Plugin binnen einer Fuel-Scheibe
  ab, statt die TUI einzufrieren. Abbruch meldet sich als `SeppError::Aborted` (bestehender
  Budget-/Abbruchpfad), Zeitbudget-Überschreitung als verwertbares Tool-Result beim LLM.
- **Hartes Speicherlimit:** `StoreLimits`-ResourceLimiter deckelt den linearen Speicher auf
  `max_memory_pages`; ein `memory.grow` über dem Limit liefert dem Plugin regulär `-1`
  (kein Trap), das Host-RSS bleibt flach.
- **Budgetierter Lade-Pfad:** Auch Instanziierung (Start-Sektion) und `sepp_spec` beim
  Discovery laufen unter Fuel- und Wanduhr-Budget (hart gedeckelt auf 5 s, da es beim Start
  keinen Abbruchkanal gibt) — ein bösartiges Plugin kann den Sepp-Start nicht mehr aufhängen.

### Tests
- Acht Szenarien in `sepp-wasm` (WAT-Fixtures): Endlosschleife → Wanduhr-Budget greift, Host
  lebt · Abbruch wirkt binnen Millisekunden, auch bei `max_wall_time_ms = 0` · lange, aber
  terminierende Rechnung überlebt viele Yield-Punkte mit korrektem Ergebnis · `memory.grow`
  über dem Limit liefert `-1`, innerhalb des Limits bleibt erlaubt · ein rechnendes Plugin
  blockiert den Reactor nicht. Dazu Manifest-Parsing-Tests für `[limits]` in `sepp-policy`.

## [0.1.14] - 2026-07-12

Kleine TUI-Politur an der in 0.1.13 eingeführten Status-Bar.

### Geändert
- **Provider-Name aus der TUI-Status-Bar entfernt.** Die Statuszeile zeigt beim Warten nur noch
  `wartet · <t>` statt `wartet auf <provider> · <t>`, und das Modell-Segment rechts nur noch den
  reinen Modellnamen (`qwen3.6:27b`) ohne den `(<provider>)`-Zusatz. Der Provider-Name war an
  beiden Stellen redundant — die Bar wird ruhiger. Das dadurch ungenutzte `provider`-Feld des
  Bar-Metrik-Caches (`MetricCache`) entfällt; die übrigen Segmente (Sparkline, Kontext-Gauge,
  `m:`/`t:`, Session-Dauer) bleiben unverändert.

## [0.1.13] - 2026-07-12

Fixes der Correctness-Funde aus dem Review des 0.1.12-Release, plus neue TUI-Status-Bar.

### Hinzugefügt
- **TUI-Status-Bar mit Aktivitäts-Sparkline.** Die Statuszeile zeigt statt statischem
  „denkt …" ein Live-Diagramm der Streaming-Intensität (Token-Rate der letzten 2 s als
  ▁▂▃▅▇-Balken — flache Linie heißt ehrlich „es fließt gerade nichts", z. B. während ein
  Tool läuft) plus Zustandswort (wartet auf <provider> / denkt / antwortet / <tool> … /
  komprimiert / Abbruch / Fehler) und Turn-Timer. Rechts davon: Meldungsbereich,
  Modell (Provider), Kontext-Gauge in Prozent der Auto-Compaction-Schwelle (grün/gelb/rot;
  100 % = Compaction feuert beim nächsten Prompt), `m:`Messages, `t:`Tool-Aufrufe im Turn,
  Session-Dauer. Bei schmalen Terminals fallen Segmente von rechts weg (das Statement
  bleibt am längsten). `/hide`/`/show` togglen die Bar wie bisher; der Animations-Tick
  (250 ms) läuft NUR während eines Turns — im Idle null zusätzliche Wakeups, und die Bar
  liest ausschließlich gecachte Metriken (nie den Session-Mutex im Render-Pfad).
  Ausgeblendetes Thinking speist Aktivität und Sparkline jetzt trotzdem.

### Geändert
- **Breaking: `--provider local` verlangt ein nicht-leeres `OPENAI_BASE_URL`** und bricht sonst
  früh mit Anleitung ab (Audit-Eintrag `missing_base_url`) — der bisherige stille Fallback auf
  api.openai.com entfällt. Grund: Seit der `OPENAI_BASE_URL=""`-Härtung in 0.1.12 wäre ein
  leerer Wert sonst kein harter Fehler mehr gewesen, sondern ein stiller Cloud-Request samt
  Prompt und `OPENAI_API_KEY`, obwohl „lokal" gemeint war. Gilt für TUI, `-p` und `--rpc`;
  wer die Cloud will, nimmt `--provider openai`.
- **Eine Env-Wert-Semantik für alle Provider (`nonempty_trimmed`):** leer/Whitespace zählt als
  fehlend, umgebender Whitespace wird entfernt — ein Trailing Space in `OPENAI_BASE_URL`
  (Copy-Paste) landete vorher als `%20` in der Request-URL (kryptisches 404), ein
  Whitespace-Key als sinnloser `Bearer`-Header (später 401 statt Frühmeldung). Der
  openai-Key-Frühcheck nutzt dieselbe Auflösung wie der Provider (vorher `var_os`-Drift:
  `OPENAI_BASE_URL=""` übersprang den Check und endete als roher 401 ohne Audit-Eintrag).
- **Start-Hinweise erreichen die TUI:** der „--think wirkungslos"-Hinweis und die
  Cross-Provider-Modellwarnung erscheinen im Chatfenster statt als eprintln hinter dem
  Alternate-Screen zu verpuffen (bei `-p`/`--rpc` weiter auf stderr).

### Behoben
- **OpenAI-Mapper: Tool-Calls mit leerer id laufen wieder** (synthetische id `call_synth_…`
  statt stummen Verwerfens — Regression aus 0.1.12, unter 0.1.11 lief der Call mit id `""`);
  und die Invariante „genau ein Start/Stop je id" hält jetzt auch bei degeneriertem
  id-Recycling (A→B→A) und Index-Drift (gleiche id unter neuem Index) — vorher doppelter
  `ToolUseStart` ohne zweites Stop → doppelte Tool-Ausführung, doppelte `tool_call_id`.
- **`/model` zieht die Auto-Compaction-Schwelle nach** (`set_model` berechnet sie aus dem
  Kontextfenster des neuen Modells, Formel zentral in `sepp_agent::default_compact_threshold`)
  — vorher blieb die Start-Schwelle stehen und ein kleineres Modell lief über, bevor je
  komprimiert wurde. Custom-Modelle erben zudem den TATSÄCHLICHEN Session-Provider
  (`AgentSession::provider_name`), nicht das Provider-Tag des Vorgängermodells.
- **TUI: `/quit` während eines laufenden Turns cancelt den Turn** (wie Ctrl+C) — vorher hing
  der Prozess nach dem Verlassen des Alternate-Screens stumm am Session-Mutex bis Turn-Ende,
  und Ctrl+C in dem Zustand killte ohne `finalize()`/fsync.
- **TUI: abgewiesene Slash-Befehle leeren die Eingabe nicht mehr** („läuft noch — bitte
  warten" bei laufendem Turn) — Parität zum Eingabe-Erhalt für normale Prompts aus 0.1.12.
- **TUI: `/reload`/`/trust` verschlucken Hook-Fehler nicht mehr:** ein Rhai-Syntaxfehler in
  einem Skript deaktivierte vorher kommentarlos ALLE Hooks (auch intakte Policy-Guards, Meldung
  „0 Hook-Quelle(n)"); jetzt rote Fehlermeldung, bestehende Hooks bleiben aktiv,
  Skills/Templates werden trotzdem aktualisiert.
- **TUI: Feedback-Meldungen überleben das Turn-Ende** — Notices (bei versteckter Statuszeile,
  plus Start-Hinweise) leben außerhalb des Transcripts, das `rebuild_transcript` bei jedem
  Turn-Ende aus den Session-Messages neu baut und das sie vorher rückwirkend löschte; sie
  gelten bis zur nächsten Nutzeraktion. Bei zurückgescrolltem Verlauf springt die Ansicht
  zur Meldung (Scroll-Reset — vorher landete sie unsichtbar unterhalb des Sichtfensters).
- **TUI: `/trust` meldet genau eine Zeile** („Projekt vertraut · <Reload-Summary>") statt bei
  versteckter Statuszeile zwei fast identische Transcript-Zeilen zu erzeugen; Reload-Fehler
  erscheinen rot statt als Info verpackt.

## [0.1.12] - 2026-07-05

Review-Härtung des 0.1.11-Umfangs (`--provider mlx`, TUI `/hide`/`/show`) — Fokus Sicherheit,
Performance und Plattform-Robustheit.

### Behoben
- **OpenAI-Streaming-Mapper trennt Tool-Calls bei Index-Recycling korrekt.** Server der
  llama.cpp-Familie (LM Studios Engine) streamen teils jeden Tool-Call erneut unter `index:0`
  mit neuer id; bisher wurden die Argumente beider Calls unter der ersten id konkateniert
  (ungültiges JSON, zweites Tool lief nie). Neues SSE-Fixture `openai_repeated_index.sse`
  deckt den Fall ab; leere Tool-Call-ids zählen jetzt wie „keine id".
- **TUI: `/show` friert die UI nicht mehr ein.** Der Handler lockte den Session-Mutex, den ein
  laufender Prompt-Task für die gesamte Turn-Dauer hält — die Event-Loop stand bis Turn-Ende
  (kein Rendern, kein Esc-Abbruch). `/hide`/`/show` togglen jetzt lock-frei.
- **TUI: getippte Eingabe geht bei laufendem Turn nicht mehr verloren** — der Text bleibt in
  der Eingabezeile stehen statt kommentarlos verworfen zu werden.
- **Leere `OPENAI_BASE_URL` (`=""`) zählt jetzt wie „nicht gesetzt"** — vorher ging der
  Preset-Default verloren (roher „relative URL"-Fehler beim ersten Request) und der
  mlx-Erreichbarkeits-Check wurde übersprungen.
- **mlx-Fehler melden sich als „mlx", nicht als „openai"** (`name()` + alle Fehlertexte) —
  LM-Studio-Probleme werden nicht mehr dem falschen Anbieter zugeschrieben.
- **Verbindungsfehler nennen den Endpunkt** („Verbindung zu … fehlgeschlagen — läuft der
  Server?") statt eines rohen reqwest-Texts — deckt auch den Fall ab, dass der Server nach dem
  Preflight stirbt.
- **TUI: Cursor-Überlauf behoben** — die Cursor-Spalte wird in `usize` gerechnet (kein
  Debug-Panic mehr bei sehr langen Eingaben/Paste), erst final geclampt.
- **Doku-Drift repariert:** `sepp --help` listet `/hide` `/show`; `--provider`-Aufzählungen
  (RunOpts-Doku, README) nennen `zai` und `mlx`; CHANGELOG-Linkblock 0.1.5–0.1.12 nachgezogen.

### Geändert
- **mlx-Preflight nicht-blockierend und IPv4+IPv6-korrekt:** async `tokio::net`-Connect mit
  700-ms-Timeout gegen den Hostnamen `localhost:1234` (getaddrinfo probiert `::1` UND
  `127.0.0.1`) statt eines synchronen IPv4-only-Syscalls auf dem Runtime-Thread; der
  Default-Endpunkt lebt jetzt als eine `pub`-Konstante in `sepp-provider` (Konsistenz per
  Unit-Test gesichert), Meldungstexte leiten sich daraus ab.
- **Hinweis, wenn `--think`/`SEPP_THINK` wirkungslos ist** (`--provider openai|local|mlx`
  senden kein Reasoning-Feld) — vorher ein stiller No-op.
- **TUI: Meldungen erreichen bei versteckter Statuszeile den Chatverlauf** (`notify`-Helfer;
  Fehler rot) — Feedback wie „läuft noch", `/model`-Ausgaben oder Befehls-Fehler verpufft
  nach `/hide` nicht mehr unsichtbar.
- **TUI: Warnung bei Prompt-Templates, die Builtin-Befehle verschatten** (beim Start und bei
  `/reload`) — solche Templates sind per Slash unerreichbar, der Builtin gewinnt.
- **TUI: `/model` mit unregistrierter ID erbt den Session-Provider** (korrekte
  Compaction-Schwelle, z. B. 128k statt fälschlich anthropic/200k) und zeigt die Modell-ID
  statt „(custom)"; die TUI-eigene `custom_model`-Kopie ist konsolidiert.
- **Provider-Tests mutieren die Prozess-Umgebung nicht mehr** (`remove_var` entfernt;
  base_url-Auflösung als pure, direkt getestete Funktion) — kein Data-Race-Fenster mehr im
  parallelen Test-Binary.

### Sicherheit
- **`--provider mlx` sendet `OPENAI_API_KEY` nur noch bei explizit gesetztem
  `OPENAI_BASE_URL`.** Im Zero-Config-Fall geht kein Bearer-Token mehr über Klartext-HTTP an
  den lokalen Port 1234 — ein für andere Tools exportierter echter OpenAI-Key kann nicht mehr
  an einen fremden lokalen Prozess oder in Server-Logs lecken. Wer LM Studio mit aktivierter
  Auth nutzt, setzt `OPENAI_BASE_URL=http://localhost:1234/v1` als bewusstes Opt-in.

## [0.1.11] - 2026-07-01

### Hinzugefügt
- **`--provider mlx` — Zero-Config-Verbindung zu lokaler MLX-Inferenz via LM Studio.** Der lokale
  OpenAI-kompatible Server von LM Studio wird ohne Konfiguration erreicht: `--provider mlx` zielt
  standardmäßig auf `http://localhost:1234/v1` (statt api.openai.com), API-Key optional. Das Modell
  wählt der Nutzer mit `-m` (passend zum in LM Studio geladenen Modell) — sepp gibt kein Modell vor.
  Ist der Server nicht erreichbar oder fehlt `-m`, bricht sepp früh mit einer hilfreichen Meldung ab
  statt mit einem rohen Connection-Fehler. `OPENAI_BASE_URL` überschreibt den Endpunkt (abweichender
  Host/Port).
- **TUI: `/hide` und `/show`** blenden die gelbe Statuszeile aus/ein — mehr Platz im Terminal; die
  Statuszeile wird nur eingeplant (gerendert), wenn sie sichtbar ist.

## [0.1.10] - 2026-07-01

### Hinzugefügt
- **macOS: OS-Dateisystem-Sandbox für MCP-Subprozesse via Seatbelt** (`sandbox_init`, rohes
  SBPL-Profil im `pre_exec` des Kindes). Damit erhalten stdio-MCP-Server auf macOS dieselbe
  Absicherung wie unter Linux-Landlock — Scope Dateisystem + Environment-Scrubbing, **fail-closed**
  (schlägt `sandbox_init` fehl, wird der Subprozess nicht ungesandboxt gestartet). Read- und
  Write-Confinement auf echtem macOS (26.x) verifiziert. Nur Plattformen ohne Adapter (Windows/BSD)
  fallen weiterhin auf `NullSandbox` mit Warnung zurück.

### Geändert
- **`install.sh` trägt den PATH automatisch ein.** Liegt das Zielverzeichnis (Default
  `~/.local/bin`) nicht im PATH, ergänzt der Installer idempotent eine PATH-Zeile in der zur
  Login-Shell passenden Profildatei (`~/.zprofile` / `~/.bash_profile` / `~/.profile`). Damit ist
  der macOS-Install 1:1 wie unter Linux — kein manueller PATH-Schritt mehr. System-Installationen
  (`/usr/local/bin`) sind ohnehin im PATH und bleiben unberührt.

### Entfernt
- **Token-Verbrauch-Anzeige komplett entfernt.** Die Mini-Tabelle am Ende der Konversation
  (One-shot/TUI), die maschinenlesbare `usage_summary`-Zeile im RPC-Modus und der persistierte
  `usage_summary`-Eintrag in der Session-Datei entfallen samt der internen kumulativen
  Token-Buchhaltung (`total_usage`/`turns`). Die per-Turn-`usage` an jeder Assistant-Nachricht
  bleibt erhalten (Provider-Daten); `last_usage` bleibt als Basis der Auto-Compaction-Schwelle und
  `model_label` weiterhin für die TUI-Statuszeile. Alte Sessions mit `usage_summary`-Einträgen
  bleiben les- und ladbar (generischer Custom-Eintrag).

## [0.1.9] - 2026-06-29

### Geändert
- **FHS-Layout: die globale Wurzel ist in `config_root` und `state_root` getrennt.** config_root
  (`settings.toml`, `skills/`, `prompts/`, `hooks/`, `plugins/`): `$SEPP_CONFIG_DIR` → `$SEPP_HOME`
  → vorhandenes `~/.sepp` → vorhandenes `/etc/sepp` → `~/.sepp`. state_root (`sessions/`,
  `trust.json`): analog mit `$SEPP_STATE_DIR` und `/var/lib/sepp`. **Default bleibt die eine Wurzel
  `~/.sepp`**; der Split greift nur, wenn die Env-Variablen gesetzt sind oder ein System-Setup
  existiert. `SEPP_HOME` setzt weiterhin beide Wurzeln (rückwärtskompatibel).
- **Sessions liegen wieder zentral** unter `state_root/sessions/<hash(cwd)>/` (kehrt die
  projektlokale Ablage aus 0.1.8 um). Projektlokales `<repo>/.sepp` enthält jetzt **nur Config**
  (skills/prompts/hooks/plugins/settings.toml); `sepp init` legt dort kein `sessions/` und keine
  `.gitignore` mehr an.

### Hinzugefügt
- **`sepp init --system`**: legt das FHS-Layout in einem Befehl an (`/etc/sepp` config +
  `/var/lib/sepp` state, state_root `0700`) und nennt die passenden Env-Exports. Über
  `$SEPP_CONFIG_DIR`/`$SEPP_STATE_DIR` umlenkbar.
- **`install.sh --system`**: installiert die Binary nach `/usr/local/bin` und ruft `sepp init
  --system` — Systeminstallation in einem Schritt.
- **`sepp uninstall --purge` räumt beide Wurzeln** (config_root + state_root) plus projektlokale
  `.sepp` via Trust-Registry. `install.sh --uninstall` delegiert nun an die Binary (behebt, dass es
  vorher `~/.sepp` hartkodierte und `SEPP_HOME` ignorierte).

## [0.1.8] - 2026-06-29

### Geändert
- **`sepp uninstall --purge` entfernt jetzt auch projektlokale `.sepp`-Verzeichnisse.** Neben dem
  globalen Root (`~/.sepp`/`$SEPP_HOME`, enthält Keys/Trust) werden alle projektlokalen `.sepp`
  entfernt, die `sepp init` in der Trust-Registry (`trust.json`) vermerkt hat — standortunabhängig
  (z. B. `/home/.sepp`, egal aus welchem Verzeichnis `uninstall` läuft). Vorher traf `--purge` nur
  den globalen Root, sodass projektlokale Installationen verwaist zurückblieben. Jede Aktion wird
  einzeln gemeldet; entfernt werden ausschließlich `…/.sepp`-Unterordner, nie die Projektordner.
- **Sessions liegen jetzt projektlokal** unter `<repo>/.sepp/sessions/<uuid>.jsonl` (vorher global
  `~/.sepp/sessions/<hash(cwd)>/`). Dadurch reisen Session-Logs mit dem Projekt. **`SEPP_HOME`
  verschiebt Sessions nicht mehr** (steuert weiterhin globale Config/Resources/Trust). Alte globale
  Sessions werden von `-c`/`-r` nicht mehr gefunden (keine Migration — Logs sind ephemer).
- **Token-Live-Anzeige in der TUI-Statuszeile entfernt** — sie zeigt nur noch das Modell. Der
  detaillierte Token-Verbrauch erscheint stattdessen als Mini-Tabelle am Ende der Konversation.

### Hinzugefügt
- **`sepp init` legt `sessions/` und eine `.gitignore` mit an** (idempotent). Die `.gitignore`
  schützt projektlokale Laufzeitdaten (Session-Logs, `trust.json`, SQLite) vor versehentlichem
  Commit; das Config-Skelett bleibt teilbar.
- **Audit jeder Start**: Der Session-Store wird vor der API-Key-Prüfung gebaut. Bricht der Start ab
  (z. B. fehlender Key), wird ein `aborted`-Eintrag geschrieben und fsync't — die Session-Datei
  existiert also auch bei fehlgeschlagenem Start. Provider-Fehler mitten in der Konversation flushen
  jetzt ebenfalls (Audit-Trail durabel).
- **Session-weite Token-Buchhaltung**: kumulative Summe (Input/Output/Cache) über alle Turns, am
  Ende der Konversation als `usage_summary`-Eintrag in der Session-Datei persistiert und als
  Mini-Tabelle angezeigt (One-shot/RPC → stderr, TUI → beim Quit). RPC emittiert beim Shutdown eine
  maschinenlesbare `usage_summary`-Zeile.

## [0.1.7] - 2026-06-29

### Geändert
- **`sepp init` legt die Konfig jetzt projektlokal an** (`<cwd>/.sepp`) statt global in `~/.sepp`
  und vertraut das Verzeichnis automatisch, damit es sofort geladen wird. Für die globale Wurzel:
  `sepp init --global`. **Achtung: Default-Verhalten geändert** — wer das alte Verhalten will,
  nutzt `--global`.

### Hinzugefügt
- **`SEPP_HOME`** verlegt die globale Konfig-Wurzel konsistent für Anlegen, Laden und Trust
  (Default `~/.sepp`, Konvention wie `CARGO_HOME` — der Wert ist direkt die Wurzel). Behebt, dass
  die Konfig als root unter `/root/.sepp` landete.

## [0.1.6] - 2026-06-29

### Geändert
- **z.ai ist jetzt ein eigenständiger Connector** (`ZaiProvider`, Modul `sepp-provider::zai`,
  Feature `zai = ["openai"]`) statt eines Dialekt-Flags auf dem OpenAI-Adapter. `name()` liefert
  `"zai"`, und alle Fehler-/Stream-Texte tragen `zai:` statt `openai:` — ein z.ai-Fehler erschien
  vorher fälschlich als OpenAI-Fehler. Das OpenAI-kompatible Drahtformat (SSE-Decoder,
  Request-Builder) wird weiterhin geteilt; dupliziert wird nichts.

### Behoben
- **Falsches Endpunkt-Routing bei GLM-Modellen.** Ohne `--provider`/`SEPP_PROVIDER` wird der
  Provider nun aus dem Modell abgeleitet (`-m glm-5.2` → `zai`). Bisher konnte ein GLM-Modell an
  `api.openai.com` gesendet werden und scheiterte dort am 401 („You didn't provide an API key").
  Die Mismatch-Warnung greift jetzt auch für GLM-Modelle auf `--provider local/openai` (vorher
  stillschweigend unterdrückt).
- **Sicherheits-Advisory `anyhow`** auf `1.0.103` angehoben (RUSTSEC-2026-0190: Unsoundness in
  `Error::downcast_mut()`). `cargo deny check` ist damit wieder grün und der Release-Build läuft.

### Tests
- **z.ai Live-Smoke-Test** (`crates/sepp-provider/tests/zai_live.rs`). Per Default `#[ignore]`;
  läuft nur über `just test-live` mit gesetztem `ZAI_API_KEY` und macht einen minimalen echten
  Call gegen api.z.ai (kein `Error`-Event, sauberer MessageStart…MessageStop, etwas Text). Ohne
  Schalter/Key ein stiller No-op.

## [0.1.5] - 2026-06-29

### Hinzugefügt
- **z.ai / Zhipu-GLM als Provider** (`--provider zai` bzw. `SEPP_PROVIDER=zai`). Nutzt den
  OpenAI-kompatiblen Endpunkt `https://api.z.ai/api/paas/v4` über den bestehenden OpenAI-Adapter —
  kein neuer Parser. Key aus `ZAI_API_KEY` (Format `id.secret`), Endpunkt über `ZAI_BASE_URL`
  überschreibbar (z. B. China-Region). GLM-5.2/4.6/4.5-Air/4.5-Flash sind in der Modell-Registry
  hinterlegt (Default-Modell `glm-5.2`, das aktuelle Flaggschiff); Kontextfenster/Limits sind
  konservativ und gegen die z.ai-Docs zu verifizieren. Fehlt der Key, scheitert der Start mit
  einem hilfreichen Hinweis.
- **OpenAI-Adapter: `reasoning_content` → ThinkingDelta.** Reasoning-Modelle über
  OpenAI-kompatible Endpunkte (z. B. GLM-5.2/4.6, DeepSeek-R1) streamen ihr Denken im Feld
  `reasoning_content`; das wird jetzt als Thinking abgebildet statt verworfen (No-op für reine
  Chat-Modelle).
- **Reasoning-Steuerung.** `--think`/`--no-think` und `SEPP_THINK` (gelayert wie `SEPP_PROVIDER`,
  Flag gewinnt) schalten das Denken ein/aus; bei `--provider zai` (GLM) ist Reasoning **per Default
  an**, andere Provider bleiben unverändert. Der z.ai-Adapter sendet dafür `thinking:{type:…}`
  (binär, nur am z.ai-Endpunkt; explizit `disabled` spart bei Trivialfragen ~Faktor 77
  completion_tokens). Anzeige gedimmt sichtbar (Opt-out `--hide-thinking`): One-shot streamt das
  Denken nach **STDERR** (stdout bleibt reiner Datenkanal), die TUI zeigt es gedimmt im Verlauf,
  RPC liefert weiterhin `{"type":"thinking"}`. Hinweis: das Denken (Chain-of-Thought) wird wie die
  Antwort in der Session-JSONL persistiert; an die Provider zurückgespielt wird es nicht.

## [0.1.4] - 2026-06-28

### Hinzugefügt
- **`sepp uninstall`** entfernt die installierte Binary direkt aus sich selbst; mit `--purge`
  zusätzlich `~/.sepp` (Sessions + Config). Ohne `--purge` bleiben die Nutzerdaten bewusst stehen.
- **`install.sh --uninstall`** (optional `--purge`) als Shell-Weg für denselben Zweck — nützlich,
  wenn die Binary bereits entfernt wurde. Der Installer parst Argumente jetzt über eine echte
  Schleife (Kombinationen wie `--uninstall --purge` in beliebiger Reihenfolge); unbekannte Flags
  werden nun als Fehler gemeldet statt ignoriert.
- **`sepp init`** legt das Konfigurations-Skelett `~/.sepp/{skills,prompts,hooks,plugins}/` samt
  kommentierter Beispiel-`settings.toml` an. Idempotent — vorhandene Dateien bleiben unberührt.
- **Erst-Start-Hinweis:** Fehlt bei Default-Provider Anthropic der `ANTHROPIC_API_KEY`, erklärt eine
  mehrzeilige Meldung jetzt die Optionen (Key setzen · `--provider local`/`OPENAI_BASE_URL` · OpenAI)
  und verweist auf `~/.sepp` bzw. `sepp init`.

## [0.1.3] - 2026-06-26

### Geändert
- **Lizenz von Apache-2.0 auf PolyForm Noncommercial 1.0.0 umgestellt.** `sepp mini` ist damit
  *source-available* und darf **ausschließlich für nicht-kommerzielle Zwecke** genutzt werden.
  Der Patent-Grant gilt nur für diese erlaubte Nutzung; ein kommerzieller Patent-Grant wird nicht
  gewährt. Betrifft `LICENSE`, `NOTICE`, die `Cargo.toml`-Metadaten, `README.md` und
  `CONTRIBUTING.md`. Für kommerzielle Nutzung bitte den Autor kontaktieren.
- `cargo-deny`-Allowlist um `PolyForm-Noncommercial-1.0.0` ergänzt (für die eigenen
  Workspace-Crates), damit das Supply-Chain-Gate grün bleibt. Die Allowlist für
  Abhängigkeits-Lizenzen (u. a. `Apache-2.0`) bleibt unverändert.

> Hinweis: Der frühere Release `v0.1.0` bleibt unter Apache-2.0 lizenziert. Die Umstellung gilt
> ab `v0.1.3`.

## [0.1.0] - 2026-06-24

Erste öffentliche Version. Funktional vollständig und getestet.

### Hinzugefügt
- **Agent-Kern** (`sepp-core`, `sepp-provider`, `sepp-tools`, `sepp-agent`): Streaming-Loop mit
  parallelem Tool-Dispatch (tokio `JoinSet`), Cancellation, Kontext-Budget und Auto-Compaction.
  Eingebaute Tools `read`/`write`/`edit`/`bash` mit verpflichtender Output-Trunkierung und
  pro-Pfad serialisierten Datei-Mutationen.
- **Anthropic-Provider** (Messages API) mit handgeschriebenem SSE-Decoder (gegen Fixtures getestet).
- **Interaktive TUI** (ratatui/crossterm) mit Slash-Commands (`/new` `/resume` `/tree` `/compact`
  `/model` `/trust` `/reload` …) sowie **One-shot** (`-p`).
- **Persistente Baum-Sessions** als JSONL (Default) mit Branching und Compaction; optional
  **SQLite**-Backend (`--features sqlite`, WAL).
- **Erweiterbarkeit (4 Tiers):** Resources (Skills→System-Prompt, Prompt-Templates→Slash-Commands),
  Hooks (Rhai), WASM-Plugins (capability-gated, via `wasmi`), MCP-Server (rmcp-Client als Tool-Quelle).
- **Sicherheitsmodell:** `sepp-policy` mit `Capability`/`Policy`, Manifest-Parser, OS-Sandbox via
  **Landlock** (fail-closed, wenn nicht durchsetzbar) und Environment-Scrubbing für Subprozesse;
  Secret-Broker; projektlokale Erweiterungen erst nach Trust.
- **Native Sub-Agenten:** isolierter Kontext, eingeschränktes Toolset, eigenes Budget.
- **Multi-Provider:** OpenAI-kompatibler Adapter (inkl. lokaler Endpunkte via `OPENAI_BASE_URL`),
  Auswahl per `--provider` / `SEPP_PROVIDER`.
- **JSONL-RPC-Modus** (`--rpc`) zum Einbetten in andere Programme — selber Kern wie TUI/One-shot.
- **Distribution:** statische Binaries (CI-Matrix Linux musl + macOS), `install.sh`,
  `cargo audit` + `cargo deny` in CI.

### Sicherheit
- Subprozesse (MCP, `bash`) erben keine API-Keys mehr (Environment-Scrubbing bzw. gezieltes
  Entfernen von Provider-Keys).
- Landlock verfährt fail-closed, wenn der Kernel die Durchsetzung nicht garantiert.
- MCP- und WASM-Tool-Ausgaben werden vor dem Kontextfenster getrunkt; WASM-Rückgaben und der
  SSE-Decoder sind gegen unbegrenztes Speicherwachstum abgesichert.

[Unreleased]: https://github.com/Vezir0013/sepp-mini/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.22...v0.2.0
[0.1.22]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.21...v0.1.22
[0.1.21]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.20...v0.1.21
[0.1.20]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.19...v0.1.20
[0.1.19]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.18...v0.1.19
[0.1.18]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.17...v0.1.18
[0.1.17]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.16...v0.1.17
[0.1.16]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.15...v0.1.16
[0.1.15]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.14...v0.1.15
[0.1.14]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.13...v0.1.14
[0.1.13]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.12...v0.1.13
[0.1.12]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.11...v0.1.12
[0.1.11]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.10...v0.1.11
[0.1.10]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Vezir0013/sepp-mini/compare/v0.1.0...v0.1.3
[0.1.0]: https://github.com/Vezir0013/sepp-mini/releases/tag/v0.1.0
