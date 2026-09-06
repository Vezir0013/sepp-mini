# Changelog

Alle nennenswerten Änderungen an diesem Projekt werden hier dokumentiert.

Das Format orientiert sich an [Keep a Changelog](https://keepachangelog.com/de/1.1.0/),
und das Projekt folgt [Semantic Versioning](https://semver.org/lang/de/).

## [Unreleased]

### Behoben
- **Ein Fehler im Hook sah aus wie „kein Hook".** Rhai meldet „Funktion nicht gefunden" für zwei
  sehr verschiedene Dinge: Das Skript definiert den Handler nicht — dann ist Überspringen
  richtig — oder der Handler existiert und ruft in seinem Rumpf etwas Falsches auf. Geprüft
  wurde nur die Fehlerart, nicht ihr Inhalt, und beides galt als „kein Handler". Ein Tippfehler
  wie `handled("x")` (die Funktion nimmt kein Argument) schaltete den Hook damit stumm ab; das
  Modell antwortete normal, niemand erfuhr etwas. Rhai unterscheidet die Fälle sehr wohl, in
  Name **und** Position — beides wird jetzt geprüft, und ein gescheitertes Skript meldet sich
  mit Dateiname, Handler und Zeile. Die Arbeit läuft dabei weiter; nur ein gescheiterter
  `on_tool_call` lässt sein Werkzeug ausfallen, weil er `block` hätte sagen können.
  Ebenfalls neu gefangen, weil zur Laufzeit nicht unterscheidbar: ein Handler mit falscher
  Parameterzahl und ein vertippter Handler-Name — beides meldet sich beim Laden.
- **Fremder Text ging roh ans Terminal.** Paketbeschreibungen, Rechte-Pfade, Hostnamen,
  Dateinamen aus einem Archiv, Registry-Beschreibungen und Werkzeug-Ergebnisse wurden
  unverändert ausgegeben — ausgerechnet im Zustimmungsdialog, direkt neben der Frage „Rechte
  gewähren?". Ein Wagenrücklauf in der Beschreibung löscht die Zeile darüber und schreibt eine
  harmlose an ihre Stelle; ein Rechts-nach-links-Zeichen lässt `gro.esiob` als `boise.org`
  erscheinen. Solcher Text wird jetzt bereinigt: Gefährliche und unsichtbare Zeichen werden
  durch ein **sichtbares** Ersatzzeichen ersetzt, nicht entfernt — wer sie löscht, macht einen
  manipulierten Namen von einem harmlosen ununterscheidbar. Betroffen sind `sepp pkg`,
  `sepp audit`, `sepp policy` und die Startmeldungen ohne TUI; in der Oberfläche selbst war es
  nie ein Problem, weil ratatui Steuerzeichen ohnehin verwirft. Eine sehr lange
  Paketbeschreibung wird zusätzlich gekürzt — auch sie kann die Rechteliste aus dem Bild
  schieben. `check_url_scheme` lehnt Whitespace, Steuer- und Formatzeichen jetzt selbst ab.
- **Ein überlasteter Anbieter kostete einen ganzen Turn.** Jeder Nicht-2xx brach den Prompt sofort
  ab — ein `429` (Ratenlimit) oder ein `529` („overloaded", bei Anthropic unter Last regelmäßig)
  beendete die Arbeit mit einer Fehlermeldung, und der Mensch musste von Hand erneut senden. Jetzt
  wird bei `429`, `408` und `5xx` bis zu dreimal versucht: 1 s, dann 2 s mit Zufallsanteil, wobei
  ein `Retry-After` des Servers Vorrang hat und auf 30 s gedeckelt wird — länger stillzustehen ist
  von einem Hänger nicht zu unterscheiden. Wiederholt wird ausschließlich, solange **kein Byte des
  Streams geflossen ist**; ein Abbruch mitten im Stream würde sonst Text verdoppeln. Ein
  Transportfehler (Verbindung abgelehnt, DNS) wird nicht wiederholt: Dort ist die Adresse falsch
  oder der Dienst aus. Und der Wiederanlauf ist **sichtbar** — in der TUI in der Statuszeile, im
  One-shot auf stderr, im RPC als `notice`-Zeile —, damit die Verzögerung erklärt ist statt
  rätselhaft.
- **Ctrl+C wirkte nicht, bevor das erste Byte da war.** Der Abbruch wurde erst im laufenden Stream
  beachtet; `send()` und das Lesen des Fehler-Bodys waren blind dafür. Bei einem toten Endpunkt
  (falsche `OPENAI_BASE_URL`, LM Studio nicht gestartet) hing sepp am OS-Timeout, das Minuten
  dauern kann. Jetzt ist der Verbindungsaufbau auf 10 s begrenzt und jede Wartephase abbrechbar,
  auch der Backoff. Ein **Lese**-Timeout gibt es bewusst nicht: Reasoning-Modelle schweigen vor
  dem ersten Token teils minutenlang, und ein Deckel darauf würde genau die langen, teuren
  Antworten abschneiden.
- **Der Fehler-Body eines Anbieters war ungedeckelt.** Eine Megabyte-Antwort auf einen abgelehnten
  Request wanderte vollständig in die Fehlermeldung und damit ins Terminal und ins Kontextfenster.
  Jetzt sind es höchstens 64 KiB; das Meldungsformat bleibt unverändert.
- **Ein Abbruch während der Compaction galt als Erfolg.** `summarize()` prüfte den
  `CancellationToken` nicht und lieferte die bis dahin gesammelte, abgeschnittene Zusammenfassung
  zurück — die dann echte Nachrichten ersetzt hätte. Jetzt meldet sich der Abbruch als solcher.

### Hinzugefügt
- **Hook-Meldungen erreichen die Oberfläche.** `notify(…)` schrieb bisher nur ins Log und war
  damit überall unsichtbar: In der TUI gibt es keinen Log-Empfänger, im One-shot liegt die
  Meldung unter der Standardschwelle. Jetzt geht sie denselben Weg wie ein Hinweis des Agenten
  und erscheint in TUI, One-shot und RPC. Der Start meldet außerdem, wie viele Hook-Skripte
  geladen wurden — bisher war nur der Fehlerfall sichtbar.

### Geändert
- **RPC kennt ein weiteres Ereignis:** `{"type":"notice","text":…}` meldet einen Wiederanlauf.
  Additiv wie alle bisherigen Erweiterungen — ein Client, der es nicht kennt, ignoriert die Zeile.
  Die Stream-Invariante lautet jetzt `Notice* MessageStart … Usage? MessageStop`.
- **Das Supply-Chain-Gate meldet auch transitive Befunde.** `cargo-deny` bewertet
  `unsound`- und `unmaintained`-Advisories seit einer Änderung seiner Standardwerte nur noch für
  Abhängigkeiten, die ein Workspace-Crate direkt nennt. Zwei Befunde zu `lru`
  (RUSTSEC-2026-0002, RUSTSEC-2026-0253), die transitiv über ratatui hereinkamen, blieben
  deshalb in CI stumm, obwohl `cargo audit` sie nannte — ein Gate, das nur die erste Ebene
  ansieht, sagt nichts über die Lieferkette. `deny.toml` setzt beide Felder jetzt ausdrücklich
  auf „all"; jede Kette zählt. Jeder Ignore trägt seinen Grund maschinenlesbar, damit die
  Ausgabe des Gates ihn nennt und nicht nur die Datei.
- **Die Oberfläche läuft auf ratatui 0.30.** Damit verschwinden die beiden `lru`-Befunde und die
  unmaintained-Meldung zu `paste` nicht durch einen Ignore, sondern aus dem Abhängigkeitsbaum:
  ratatui-core zieht `lru` 0.18.4 (beide Advisories dort behoben) und `paste` kommt gar nicht
  mehr vor. `deny.toml` braucht deshalb nur noch **einen** Ignore (`smartstring`, transitiv über
  rhai). Am Code der Oberfläche änderte das keine Zeile — die benutzten Bausteine (Layout,
  Block, Paragraph, Line/Span, Style) sind dieselben geblieben. Aufgenommen sind nur die
  Features, die sepp wirklich benutzt: ohne `all-widgets` (Kalender-Widget samt `time`) und ohne
  die Makro-Crate; der Layout-Cache bleibt an. `crossterm` geht im selben Zug auf 0.29, damit
  nicht zwei Fassungen desselben Terminal-Treibers im Binary landen. Unterm Strich sechs Crates
  mehr im Baum, drei Advisories weniger.

### Tests
- Neunzehn Fälle für die beiden Befunde oben: das Rhai-Verhalten beider „nicht gefunden"-Fälle
  festgenagelt (es gehört einer fremden Crate, ein stiller Wechsel würde Hooks wieder verstummen
  lassen), Tippfehler im Handler mit Skriptname gemeldet, fehlender Handler weiterhin still,
  falsche Parameterzahl und vertippter Name beim Laden, eine Meldung je Sitzung statt je Aufruf,
  ein Ende-zu-Ende-Lauf über den echten Loop; dazu harmloser Text bleibt unverändert (der
  wichtigste Fall), ein vollständiger Angriff auf den Zustimmungsdialog lässt die Rechteliste
  unversehrt, keine Eintragsart schmuggelt etwas in die Audit-Spur, und sechs neue schlechte
  URLs.
- Sieben Fälle gegen lokale `TcpListener` (Wiederanlauf mit Hinweis vor `MessageStart`, dreimal
  `503`, `401` ohne zweiten Versuch, Abbruch im Backoff und beim Warten auf die Antwort,
  gedeckelter Fehler-Body, der geteilte OpenAI-Pfad) plus acht reine Funktionen für Backoff,
  `Retry-After` (Sekunden und HTTP-Datum) und die Wahl der Statuscodes. Die Test-Server senden
  `Retry-After: 0`, damit kein Test echte Sekunden verbringt.

## [0.5.2] - 2026-09-06

Acht Befunde eines Reviews, ein gemeinsamer Nenner: Der Host hielt eine Zusage nur meistens. Der
Nachweis in der Spur gehörte ihm nur dann, wenn zufällig eine HTTP-Anfrage lief. Die
Verbrauchsgrenzen eines Plugins schrieb dessen Autor selbst — und traf damit ausgerechnet den
einen Punkt, an dem Ctrl+C geprüft wird. Ein Abbruch mitten im Werkzeug ließ die Sitzung in einem
Zustand zurück, den kein Anbieter mehr annimmt. Und `--purge` löschte ungefragt, was `SEPP_HOME`
gerade bedeutete, nachdem es die Binary schon entfernt hatte. Nichts davon ändert ein Format:
Manifeste, die in 0.5.1 luden, laden weiter; die Session-JSONL, das Paketformat und das Plugin-ABI
bleiben, wie sie sind. Was sich ändert, ist, dass die Zusagen jetzt immer gelten.

### Behoben
- **`sepp uninstall --purge` löschte ungefragt, was die Wurzeln gerade bedeuten.** `SEPP_HOME`
  **ist** die Wurzel, nicht ihr Elternverzeichnis: `SEPP_HOME=$HOME` (die Variable heißt nun mal
  „HOME") oder `SEPP_CONFIG_DIR=/etc` genügte, und `--purge` ließ `remove_dir_all` darauf los —
  ohne Rückfrage, ohne Plausibilitätsprüfung. Und die Binary war zu dem Zeitpunkt schon weg: Sie
  wurde als Allererstes entfernt, noch vor jeder Prüfung. Jetzt ist die Reihenfolge umgedreht —
  Ziele bestimmen, prüfen, Vorschau zeigen, fragen, löschen, **Binary zuletzt**. Wer „nein" sagt,
  behält sein sepp. Verdächtige Ziele werden übersprungen und mit Grund genannt, der Rest läuft
  durch: `/`, flache Pfade wie `/etc`, das Home-Verzeichnis selbst, alles, was das aktuelle
  Verzeichnis enthält, und jedes Verzeichnis ohne sepp-Merkmal (weder `.sepp` noch
  `settings.toml`, `policy.toml`, `trust.json`, `sessions/`, `pkg/` darin). Verglichen wird
  kanonisch, damit ein symlinktes `SEPP_HOME` nicht daran vorbeikommt.
  Die Rückfrage liest `/dev/tty`, wenn stdin keine Terminal ist — sonst bliebe der dokumentierte
  Weg `curl … | sh -s -- --uninstall --purge` ungefragt. Ohne jedes Terminal bricht der Vorgang
  ab und nennt `--yes`, das neue Flag für Skripte. Der Fallback in `install.sh` (Binary schon
  weg) löschte bisher selbst mit `rm -rf` und umging jede Prüfung; er hat jetzt dieselben Grenzen
  und dieselbe Rückfrage.
- **Ein Plugin bestimmte seine Verbrauchsgrenzen selbst.** `[limits]` im Manifest kam ungeprüft
  beim Host an. Das trifft die einzige Stelle, an der der WASM-Host Abbruch und Wanduhr prüft:
  den Yield-Punkt des Fuel-Slicings. Mit `fuel_slice = u64::MAX` gibt es ihn nie — Ctrl+C wirkte
  nicht mehr, der Rechen-Thread lief weiter. Schlimmer noch im Ladeweg: Dort tankt der Host
  `START_FUEL.max(fuel_slice)` in die **nicht resumierbare** Start-Sektion, ein Manifest konnte
  also den Start unterbrechungsfrei aufhängen, bevor überhaupt ein Abbruchkanal existiert. Und
  `max_memory_pages` durfte bis 65 536 (4 GiB) fordern — je Aufruf, und Aufrufe laufen parallel.
  Der Host kappt jetzt an einer Stelle (`Limits::clamped_to_host`, gerufen im einen Trichter
  `load_named`): `fuel_slice` auf 10 000 000 (genau `START_FUEL`, damit das Einmal-Budget des
  Ladewegs eine Konstante bleibt), `max_memory_pages` auf 4096 (256 MiB), `max_http_requests` auf
  1024. Jede Kappung erscheint als Startmeldung, damit der Autor sie merkt. **Gekappt, nicht
  abgelehnt:** `manifest.toml` ist ein stabiles Format, und was in 0.5.1 lud, muss weiter laden;
  die Decken wurden nachgerüstet. `max_wall_time_ms = 0` (unbegrenzt) bleibt ausdrücklich
  erlaubt — es ist genau dann sicher, wenn Yield-Punkte garantiert sind, und die garantiert erst
  der Deckel auf `fuel_slice`.
- **`host_log` sammelte im Host-Speicher.** Jede Log-Zeile eines Plugins wanderte zusätzlich in
  einen `Vec` im `HostState`, den niemand je auslas. `StoreLimits` deckelt nur den Modulspeicher,
  nicht die Puffer des Hosts: Eine Schleife aus `host_log` ließ den Host-RAM wachsen, bei
  `max_wall_time_ms = 0` beliebig lange. Der tote Akkumulator ist weg, die Zeile geht nur noch
  ins Log.
- **Ein Werkzeug konnte sich seinen eigenen Nachweis schreiben.** Der Agent-Loop übernimmt ein
  Objekt unter `details["audit"]` mit einem `kind`-Feld als Session-Eintrag — das ist der
  Nachweis, was in einem Turn entschieden wurde. Ein WASM-Plugin und ein MCP-Server liefern ihr
  Ergebnis aber als fremdes JSON und durften diesen Schlüssel setzen: Beide konnten sich eine
  Guard-Entscheidung erfinden, die in `sepp audit` von einer echten nicht zu unterscheiden war
  und die `/tree` als Guard-Eintrag sogar ausblendet. Beim Plugin griff der Host nur, wenn im
  selben Aufruf eine HTTP-Anfrage lief; ohne Netz blieb der gefälschte Eintrag unangetastet.
  Reserviert ist jetzt ein **Namensraum** statt eines Schlüssels: `audit` und `guard` gehören dem
  Host, stehen im Vertrag (`sepp_core::RESERVED_DETAIL_KEYS`, dazu
  `ToolResult::strip_reserved_details`) und werden an jeder Grenze entfernt, an der fremdes JSON
  zu einem Ergebnis wird — im WASM-Host direkt nach dem Einlesen, im MCP-Client beim Bau des
  Ergebnisses. Dass ein gefälschtes `details["guard"]` heute nichts bewirkt, liegt allein daran,
  dass es niemand ausliest; genau diese Konstellation war der Fehler bei `audit`. Die übrigen
  Felder des Werkzeugs bleiben unberührt, der Versuch wird geloggt und beim Plugin zusätzlich im
  Audit-Eintrag des Hosts vermerkt (`stripped_plugin_keys`), und kein Werkzeug fällt deshalb aus.
  Die eingebauten Werkzeuge und der Sub-Agent dürfen die Schlüssel weiter setzen; ihr Code steht
  in diesem Repo.
- **Ein Abbruch mitten im Werkzeug machte die Sitzung unbrauchbar.** Esc oder Ctrl+C während
  `bash` lief, ließ die Assistant-Nachricht mit dem `tool_use` in der Session zurück, ohne ein
  `tool_result` dazu — jeder Anbieter lehnt den nächsten Request damit ab (Anthropic 400
  „tool_use ids were found without tool_result blocks"), bis `/new`. Jetzt bekommt jeder
  offene Aufruf ein Fehler-Ergebnis „Abgebrochen …", das wie jedes andere aufgezeichnet und
  durabel gesichert wird, und erst dann kommt der Abbruch. Ebenso reißt ein Werkzeug, dessen
  Task stirbt (Panik), nicht mehr den ganzen Turn mit: Sein Platz wird zum Fehler-Ergebnis
  „Tool-Task fehlgeschlagen", die übrigen Aufrufe des Batches behalten ihre Ergebnisse.
- **Kontextüberlauf mitten im Werkzeug-Loop.** Die Auto-Compaction lief nur vor einem neuen
  Prompt; bis zu 50 Turns mit je bis zu 50 KiB Ergebnis sprengten das Fenster lange davor, und
  die Zusammenfassung selbst schickte den vollen Verlauf — zu groß, also 400, und jeder weitere
  Prompt lief in dieselbe Wand. Jetzt prüft der Loop die Schwelle auch nach jedem
  Werkzeug-Ergebnis (die einzige Stelle, an der ein Schnitt kein `tool_use` ohne Ergebnis
  hinterlässt), und `compact` hat drei Stufen, jede nur nach einem Fehler, der nach Überlauf
  aussieht (HTTP 400/413, „too long", „context length" …): voller Verlauf → Verlauf ohne
  Thinking und mit in der Mitte gekürzten Ergebnissen → harter Schnitt: Die jüngsten
  Nachrichten bleiben, geschnitten vor einer Assistant-Nachricht oder einem reinen
  Nutzer-Prompt, ein Hinweis nimmt den Platz der entfernten ein, und der Store bekommt einen
  `Compaction`-Eintrag, dessen `replaced_until` genau die letzte entfernte Nachricht ist —
  `path_messages()` und Speicher stimmen danach überein. Netz-, Schlüssel- und 5xx-Fehler
  kommen unverändert zurück; dort wäre ein Schnitt Datenverlust ohne Gewinn.
- **Die Sandbox der Kindprozesse endete am Dateisystem.** Ein sandboxed `bash` konnte sepp
  selbst mit `kill -9 $PPID` beenden, abstrakte Unix-Sockets anderer Prozesse ansprechen und —
  weil der eingebaute Grant `$TMPDIR` das ganze `/tmp` freigab — die Sockets des ssh-agent
  finden und mit den Schlüsseln des Nutzers signieren, obwohl `~/.ssh` verboten ist. Jetzt
  setzt Landlock zusätzlich die Scopes `Signal` und `AbstractUnixSocket` (ABI v6, Kernel 6.12;
  auf älteren Kerneln meldet der Start, dass sie fehlen), und `TMPDIR` zeigt auf Linux für sepp
  und alle Kindprozesse auf ein **privates 0700-Verzeichnis je Lauf** (`/tmp/sepp-<pid>-<zufall>`),
  das am Ende verschwindet; `$TMPDIR` in der Policy löst darauf auf. Programme, die `/tmp` fest
  verdrahten, brauchen ein ausdrückliches Recht (`sepp policy allow agent fs_write /tmp`). Auf
  macOS bleibt es beim `TMPDIR` des Nutzers (`/var/folders/…/T`, ohnehin je Nutzer und 0700): Die
  Xcode-Shims (`git`, `python3`) schreiben ihren Cache fest dorthin und scheiterten mit einem
  privaten Unterverzeichnis (auf macOS 26.3 nachgestellt); der ssh-agent-Socket liegt dort nicht
  im `TMPDIR`.
  Getragene Grenze, jetzt dokumentiert: Unix-**Pfad**-Sockets (`docker.sock`,
  `/run/user/<uid>/bus`) kann keiner der Adapter sperren — das kommt mit Egress-Proxy oder
  seccomp.
- **Seatbelt (macOS): `sysctl-read` und `mach-lookup` galten global.** Der Verdacht aus dem
  Review: ein sandboxed Prozess liest per `ps -Eww -p $PPID` die Umgebung von sepp samt
  API-Keys, und `open -a` oder `osascript` lassen launchd Prozesse **außerhalb** der Sandbox
  starten. Auf einem Mac mini mit macOS 26.3 nachgestellt (`sandbox-exec` mit dem Profil von
  sepp): `KERN_PROCARGS2` liefert dort für fremde Prozesse nur noch den Programmpfad — auch ohne
  Sandbox —, LaunchServices weist `open -a` aus jeder Sandbox zurück (−54), und ein Apple Event
  an den Finder scheitert schon am Default-deny (−600). Auf dieser macOS-Version besteht das
  Leck also nicht. Das Profil endet trotzdem mit zwei Verboten als Vorsorge für ältere
  Versionen, als letzte Regeln (sie gewinnen): `kern.procargs*` und die Dienste
  `com.apple.coreservices.launchservicesd`, `…appleevents`, `…quarantine-resolver`; `whoami`,
  `python3`, Namensauflösung und Schlüsselbund bleiben erreichbar. Ein `#[ignore]`-Test auf macOS
  (`seatbelt_hides_the_parents_environment`) hält fest, dass die Umgebung des Elternprozesses aus
  der Sandbox nicht lesbar ist.
- **Bilder in Werkzeug-Ergebnissen brachten bei Anthropic 400.** `ContentBlock::Image` ging
  per serde mit `source.kind` über die Leitung, die Messages API verlangt `source.type` —
  sobald ein MCP-Server (Screenshot-Werkzeug) ein Bild lieferte, wurde der ganze Request
  abgelehnt. Bilder und Werkzeug-Ergebnisse werden jetzt explizit ins Drahtformat gebracht
  (auch verschachtelt im `tool_result`; dort nur Text und Bild, `is_error` nur wenn gesetzt).
  Ein Modell ohne Bildverständnis bekommt statt 400 einen Textplatzhalter, der das Bild nennt;
  der OpenAI-Adapter, der Bilder in Werkzeug-Ergebnissen nicht senden kann, setzt denselben
  Platzhalter statt ein leeres Ergebnis zu liefern. Das Session-Format bleibt unverändert.

### Tests
- `sepp-cli`: verdächtige Löschziele (`/`, `/etc`, `$HOME`, Elternteil des aktuellen
  Verzeichnisses, Verzeichnis ohne Merkmal) fallen auf, echte Wurzeln kommen durch, der
  Merkmal-Test greift auf echten Verzeichnissen; `--yes`/`-y` im Parser, `--yes` ohne `--purge`
  ist ein Fehler.
- `sepp-policy`: `clamped_to_host` kappt genau die Felder über den Decken und meldet jedes im
  Klartext, lässt Defaults und `max_wall_time_ms = 0` in Ruhe; die Decken liegen nie unter den
  Defaults (sonst bekäme jedes Plugin eine Startmeldung); ein Manifest über der Decke parst
  weiterhin (die Stabilitätszusage als Test).
- `sepp-wasm`: **die Verhaltensbeweise** — ein Endlosschleifen-Plugin mit `fuel_slice = u64::MAX`
  lässt sich weiterhin nach 50 ms abbrechen (ohne die Kappung läuft derselbe Test in den
  Timeout), eine Endlosschleife in der Start-Sektion hängt das Laden nicht mehr auf, und
  `MAX_FUEL_SLICE <= START_FUEL` hält die beiden Konstanten beieinander; die Kappung erscheint in
  den Ladehinweisen.
- `sepp-core`: der reservierte Namensraum verschwindet aus fremden `details`, verschachtelte
  `audit`-Felder und alles, was kein Objekt ist, bleiben unberührt.
- `sepp-wasm`: ein Plugin ohne HTTP-Anfrage, das `details["audit"]` mit `kind = "guard"` und
  `details["guard"]` liefert, bekommt beide entzogen und funktioniert weiter.
- `sepp-mcp`: erste Testabdeckung der Ergebnis-Abbildung überhaupt (dafür als reine Funktion
  herausgezogen) — gefälschter Audit-Schlüssel weg, Text/Bild/`is_error`/Fallback unverändert.
- `sepp-agent`: Abbruch im Werkzeug → Verlauf mit Fehler-Ergebnis vollständig, Store gleich,
  nächster Prompt läuft; Panik im Werkzeug → Fehler-Ergebnis statt verlorenem Turn;
  Compaction nach einem großen Werkzeug-Ergebnis mitten im Loop; Zusammenfassung mit
  gekürztem Verlauf, wenn der volle scheitert; harter Schnitt, wenn auch der gekürzte
  scheitert, samt Gleichheit von `path_messages()` und Speicher; Einheiten: Überlauf-
  Heuristik, zeichensichere Kürzung, Kürzung erhält `tool_use`/`tool_result`-Paare,
  Schnittpunkt-Wahl, Zuordnung zum Store-Eintrag über Custom-Einträge und Compactions hinweg.
- `sepp-policy`: Seatbelt-Profil endet mit den Verboten für `kern.procargs*` und die
  launchd-Dienste (Position nach jedem Allow); Landlock-Scope-Test (`#[ignore]`, Kernel ≥ 6.12):
  Signal an einen Prozess außerhalb scheitert, an das eigene Kind gelingt; macOS-Test
  (`#[ignore]`): Umgebung des Elternprozesses aus der Sandbox nicht lesbar.
- `sepp-cli`: privates `TMPDIR` je Lauf ist frisch, 0700, exportiert, und `$TMPDIR` in der Policy
  löst darauf auf.
- `sepp-provider`: Bild oben und im `tool_result` im Drahtformat (`source.type`, kein `kind`),
  `is_error` nur wenn gesetzt, leere Textblöcke gefiltert; Platzhalter ohne Bildverständnis;
  OpenAI `text_of` nennt Bilder.

## [0.5.1] - 2026-09-06

Drei Löcher im Zaun, alle an der Release-Binary nachgestellt, alle geschlossen. Gemeinsam ist
ihnen: Der Guard entschied über etwas anderes, als danach wirkte — über einen Systempfad, der im
eigenen Prozess mehr preisgab als in der Sandbox; über ein Verzeichnis, das die Rechte des
nächsten Starts bestimmt und im Schreib-Grant des Projekts lag; über einen Host, den ein
Handparser anders las als der HTTP-Client. Die Regel, die aus allen dreien folgt und jetzt im
Code steht: **Was der Guard prüft, muss exakt das sein, was danach passiert.** Nichts davon
ändert ein Format oder eine Schnittstelle, die ein Paket, Plugin oder Skript benutzt.

### Behoben
- **`read /proc/self/environ` lieferte die API-Keys ins Modell.** Der eingebaute Agent-Grant
  `fs_read = ["system"]` enthielt `/proc`. Für Kindprozesse ist das nötig und ungefährlich —
  Landlocks Ptrace-Schranke lässt `bash` nicht an die Umgebung von sepp. Im eigenen Prozess gibt
  es diese Schranke nicht: `read` las die eigene Umgebung samt Schlüsseln ins Kontextfenster und
  in die Session-Datei. `"system"` bedeutet für `read`/`write`/`edit` und `host_fs_read` jetzt
  die Systempfade **ohne** `/proc`; die Sandbox gibt Kindprozessen `/proc` weiterhin selbst dazu.
  Wer `/proc` im Werkzeug wirklich braucht, gewährt den Pfad ausdrücklich. Dazu ist sepp selbst
  nicht mehr dumpbar (`PR_SET_DUMPABLE = 0`, vor allem anderen in `main`): kein Core-Dump mit
  Schlüsseln, und auch ohne Sandbox (`--mode yolo`, Plattformen ohne Adapter) kommt ein Kind
  derselben UID nicht über procfs an die Umgebung. Kinder selbst bleiben dumpbar (`execve` setzt
  das Flag zurück) — `ps`, Debugger und `/proc/self` in Skripten laufen wie zuvor.
- **Der Agent konnte seine eigene Projekt-Policy schreiben.** `<cwd>/.sepp/` liegt im
  Schreib-Grant `./`; im Modus `auto` schrieb `write` ohne Rückfrage eine `.sepp/policy.toml` mit
  `fs_read = ["/root"]` und `net = true`, und beim nächsten Start eines vertrauten Projekts galt
  sie — persistente Selbst-Eskalation, für `settings.toml` (MCP-Server), Hooks und Plugins
  ebenso. Zwei Schichten dagegen: Das Frontend meldet `<cwd>/.sepp` als **eingebautes
  Schreibverbot** (`read` bleibt erlaubt, das Projekt bleibt beschreibbar); und weil Landlock ein
  Verbot unter einer Gewährung für `bash` nicht ausdrücken kann, ist das **Vertrauen jetzt an den
  Inhalt** von `policy.toml`, `settings.toml`, `hooks/` und `plugins/` gebunden (SHA-256, in
  `trust.json`). Ändert sich dort etwas — durch den Agenten über `bash` oder durch den Menschen
  von Hand —, lädt sepp die projektlokale Konfiguration nicht mehr, meldet es beim Start und in
  `sepp policy`, und ein neues `/trust` bestätigt den Stand. Was sepp selbst im Auftrag des
  Menschen schreibt (`sepp policy allow`, „dauerhaft erlauben" in der TUI, `sepp init`), bindet
  das Vertrauen sofort neu — sofern die Konfiguration bis dahin die bestätigte war; sonst
  bleibt es ausgesetzt, bis `/trust` den gesamten Stand bestätigt. Skills und Prompts binden nicht: Sie ändern den System-Prompt, keine
  Rechte.
- **Host-Allowlist umgehbar über eine URL mit Backslash.** `url_host` zerlegte die URL von Hand
  (`rsplit('@')`), reqwest nach WHATWG: Für `https://evil.example\@api.example.com/x` sah das
  Net-Gate `api.example.com`, verbunden wurde mit `evil.example` — samt eingesetztem
  Secret-Header, und die Spur nannte den falschen Host. Ein Plugin mit `net = ["api.example.com"]`
  und `env = ["TOKEN"]` konnte so das Secret an einen beliebigen Host schicken. Jetzt entscheidet
  die `url`-Crate, die auch reqwest benutzt; eine URL mit Backslash vor dem Pfad, mit
  Whitespace/Steuerzeichen (der Parser entfernte sie still) oder ohne Authority wird gar nicht
  erst akzeptiert. Betrifft `host_http` (WASM), Secret-Header für http-MCP-Server und die
  Schema-Regel der Registry.

### Geändert
- `trust.json` trägt je Projekt statt `true` ein Objekt `{"config": "<sha256>"}` — additiv: ein
  älteres `sepp` liest es weiter als „vertraut", ein neueres bindet einen alten `true`-Eintrag
  beim ersten Lesen an den dann vorliegenden Stand. `/trust` nennt den Konfig-Stand
  (Kurz-Fingerprint); `sepp policy` und der Start melden eine seither geänderte Konfiguration.
- Hostnamen in `net`-Listen werden case-insensitiv verglichen (`Api.Example.com` trifft
  `api.example.com`); Domains kommen aus dem Parser in Kleinschreibung.
- Eingebaute Verbote des Frontends (config_root, state_root) werden kanonisiert wie der geprüfte
  Zugriffspfad — ein symlinktes oder relatives `SEPP_HOME` traf bisher nie.
- `sepp policy` führt das Schreibverbot auf `<cwd>/.sepp` mit eigener Erklärung statt als
  „für Kindprozesse nicht durchsetzbar"; der Start meldet es nicht bei jedem Aufruf.

### Tests
- `sepp-policy`: `url_host` differenziell gegen `url::Url` (Backslash, Tab, Newline,
  Großschreibung, `%5C`, IPv6, IDN) und die Ablehnungen; case-insensitives `Net`-Matching;
  `"system"` ohne `/proc`; `read /proc/self/environ` und `/proc/meminfo` verweigert, `/etc`
  erlaubt, ausdrücklich gewährtes `/proc/meminfo` erlaubt (Linux); Prozess nach
  `harden_process` nicht dumpbar (Linux); Schreibverbot auf `<cwd>/.sepp` bei erlaubtem Lesen
  und beschreibbarem Projekt; kanonisierte Frontend-Verbote über einen Symlink; Rückruf nach
  „dauerhaft erlauben" genau einmal.
- `sepp-cli`: Fingerprint folgt `policy.toml`, Hooks und Plugins, nicht Skills, ist
  deterministisch und für ein fehlendes Verzeichnis leer; Trust-Einträge in altem (`true`) und
  neuem Format, `Changed` bei abweichendem Stand.

## [0.5.0] - 2026-09-06

Ein Index nennt Pakete, er gewährt nichts. Bis jetzt kam ein Paket als Datei — per Mail, Download,
USB-Stick — und `sepp pkg install <datei>` prüfte es. Ab jetzt reicht der Name:
`sepp pkg install rechnungspruefung` holt das Paket aus einer **Registry**, einem signierten Index
auf einem beliebigen Webspace, den der Nutzer einmal in seiner `settings.toml` einträgt — samt
Public Key des Betreibers, gepinnt, ohne Dialog. Zwei Lagen Vertrauen: der Betreiber bürgt für die
Liste, der Herausgeber für das Paket. Alles, was Stufe 4 beim Paket prüft (Signatur, TOFU,
Zustimmung, Kollisionen, Hash je Datei), prüft es hier genauso, weil das geladene Paket exakt den
Weg einer Datei geht. Netz gibt es nur in `sepp pkg`, außerhalb des Guard — eine Handlung des
Nutzers wie `install.sh` — und nie in `sepp-pkg`, das ohne Netz und ohne async bleibt.

### Hinzugefügt
- **Registry-Index (Format 1):** `index.toml` mit `[[packages]]` (Name, Version, Beschreibung,
  Herausgeber samt Schlüssel, URL, SHA-256, Größe) und `index.sig` (Ed25519 über die rohen
  Index-Bytes, base64) neben den `.seppkg`-Dateien — statischer Webspace genügt. Prüfreihenfolge
  fail-closed: Größe → Signatur gegen den gepinnten Schlüssel → Text → Struktur. Unbekannte
  Felder werden gemeldet, nicht abgelehnt; mehrere Versionen je Name sind erlaubt.
- **`[[registries]]` in der globalen `settings.toml`** (`name`, `url`, `key`) — bewusst nicht
  projektlokal: Ein Repo darf per Trust keine Paketquelle mit eigenem Schlüssel einschleusen.
  `sepp init` schreibt ein auskommentiertes Beispiel. Schema-Regel für jede URL, auch für jedes
  Redirect-Ziel: `https://` immer, `http://` nur für `localhost`, `127.0.0.1`, `::1`.
- **`sepp pkg install <name>[@version]`** (`--registry <name>` wählt eine): Registries in
  Konfigurationsreihenfolge fragen, höchste oder exakte Version; das Paket gestreamt nach
  `<state_root>/pkg/.downloads/` laden — gedeckelt auf die Größe aus dem Index, gehasht beim
  Schreiben, bei Abweichung sofort weg — dann der Weg von `install <datei>`, plus: der
  Herausgeber-Schlüssel aus dem Index muss zum Paket passen (vor jedem Dialog), und die
  Zustimmung zeigt „Quelle: Registry … · URL". Die geladene Datei verschwindet nach jedem Ausgang.
- **`sepp pkg search [text]`**: Treffer aller (oder einer) Registries als Tabelle, je Name die
  höchste Version, installierte markiert.
- **`sepp pkg untrust <herausgeber>`**: nimmt das TOFU-Vertrauen zurück; installierte Pakete
  bleiben und werden genannt, beim nächsten Paket wird der Fingerprint neu bestätigt.
- **Betreiber-Werkzeuge:** `sepp pkg keygen --registry` (eigenes Schlüsselpaar `registry.key`/
  `.pub`, getrennt vom Herausgeber-Schlüssel) und `sepp pkg index <dir> [--out] [--name]
  [--base-url] [--key]`, das jedes `.seppkg` wie der Installer prüft, `index.toml` + `index.sig`
  reproduzierbar baut (bis auf `generated_at`) und den fertigen `[[registries]]`-Eintrag für die
  Nutzer ausgibt. Nie über vorhandene Dateien hinweg.
- **`sepp-pkg::registry`** mit der `Fetcher`-Abstraktion (eine Methode, `fetch_to_writer`): Deckel,
  Hash und Aufräumen liegen einmal im Crate und sind ohne Netz testbar. Das CLI stellt den
  HTTP-Fetcher (`pkg_fetch.rs`) auf eigener Runtime: Verbindungs- und Lese-Timeout, User-Agent,
  Deckel vor dem Puffern und beim Streamen, höchstens fünf Weiterleitungen und nur auf Ziele nach
  der Schema-Regel.
- **`installed.json` kennt die Quelle** (`source`: `datei` oder `registry:<name>`; `sepp pkg list`
  zeigt sie in der Spalte QUELLE) und führt unbekannte Felder mit, statt sie beim nächsten
  Schreiben zu verlieren.

### Geändert
- Fehlertexte, die bisher rieten, „die Datei zu löschen" (anderer Schlüssel unter bekanntem
  Namen, Hinweis nach `remove`), nennen jetzt `sepp pkg untrust <name>`.
- `sepp pkg` spricht für `install <name>` und `search` mit dem Netz — außerhalb des Guard, ohne
  Audit, wie `install.sh`; Proxy-Variablen der Umgebung gelten. Alle anderen Unterbefehle bleiben
  ohne Netz und ohne Runtime.
- Ein 0.4.0-`sepp` liest `installed.json` mit `source` weiter, verwirft das Feld aber beim
  nächsten Schreiben — informativ, kein Sicherheitsverlust.

### Tests
- `sepp-pkg`: Index (parsen, unbekannte Felder, je kaputtem Feld eine Ablehnung, Signatur
  manipuliert/falscher Schlüssel/zu groß, doppelte Einträge), Auflösung (höchste, exakte,
  fehlende Version), Suche, URL-Regeln (relativ, absolut, `..`, Query, Fragment, Loopback),
  `[[registries]]` (fehlende Datei, Doppelname, kaputter Schlüssel, http ohne Loopback, Query),
  Index bauen (reproduzierbar, sortiert, verifiziert, `--base-url`, kaputtes Paket, leer),
  Laden mit Fake-Fetcher (Hash, Größe, Aufräumen, 0700); `untrust`, Quelle im Nachweis, alte
  Nachweise laden, fremde Felder überleben.
- `sepp-cli`: HTTP-Fetcher gegen lokale Listener (Body, User-Agent, Weiterleitungen bis zum
  Limit, Weiterleitung auf `http://` außerhalb Loopback ohne Verbindung abgelehnt, Content-Length
  und Body über dem Deckel, 404, Schema-Regel vor jeder Anfrage); **End-to-End** ohne wasm32:
  packen → Index bauen → statischer Server auf 127.0.0.1 → `install demo` mit vorweggenommenen
  Antworten → Nachweis, Policy-Block, leeres `.downloads/`; `search`; Negativfälle (nicht im
  Index, fremde Registry, keine Registry, fremder Index-Schlüssel, manipuliertes Paket, gleiche
  Version); `index` schreibt beide Dateien und überschreibt nie; Parser-Formen.

## [0.4.0] - 2026-09-06

Ein Paket bringt keine Rechte mit — es bittet um sie. Bis jetzt hieß „eine Erweiterung
installieren": Dateien an vier Orte kopieren, ein Manifest lesen, die passenden Zeilen in die
`policy.toml` schreiben und hoffen, dass Loader und Guard denselben Namen meinen. Ab jetzt ist das
ein Befehl: `sepp pkg install <datei.seppkg>` prüft die Signatur des Herausgebers, zeigt, was drin
ist und welche Rechte es braucht, und schreibt sie nach Zustimmung als **markierten Block** in die
Policy des Nutzers — den `sepp pkg remove` wieder herausnimmt. Die Leitsätze aus dem Paket-Plan
gelten wörtlich: eine Rechtequelle (kein Paket enthält eine `policy.toml`), die Verzeichnisse des
Nutzers gehören dem Nutzer (Pakete leben unter `pkg/`), Content in `config_root`, Nachweise in
`state_root`, alles additiv.

### Hinzugefügt
- **Das Paketformat `.seppkg` (Format 1)** — ein zstd-komprimiertes tar mit `manifest.toml` und
  `manifest.sig` als ersten Einträgen, dann `skills/`, `prompts/`, `hooks/`, `plugins/`
  (je `<n>.wasm` + `<n>.toml`). Das Manifest nennt Name, Version, Herausgeber mit
  Ed25519-Public-Key, Variablen (`[vars.NAME]`, Art `path` oder `string`, optional Default), die
  Rechte je Plugin (`[rights.<plugin>]`) und **`[files]` mit SHA-256 je Datei**. Die Signatur
  deckt das Manifest, die Prüfung je Datei ist ein Hash-Vergleich. Unbekannte Felder werden
  gelesen, gemeldet, ignoriert; ein höheres `format` verlangt ein neueres `sepp`.
- **`sepp pkg keygen | pack | install | list | remove`** im neuen Crate `sepp-pkg` (Manifest,
  Hash, Signatur, Container, Installation gegen abstrakte Wurzeln — testbar ohne Umgebung) mit
  dünnem `pkg_cmd.rs`. `install` prüft fail-closed in dieser Reihenfolge, und **vor der
  Signaturprüfung landet kein Nutzdaten-Byte auf Platte**: Magic → Manifest und Signatur zuerst im
  Archiv → Signatur → Manifest → Vertrauen in den Herausgeber → Variablen → `[rights]` gegen das
  Plugin-Manifest → Zustimmung → Kollisionen → Entpacken in ein Staging-Verzeichnis mit Hash je
  Datei (keine Datei ohne Eintrag, kein Eintrag ohne Datei, keine Symlinks, kein `..`, Deckel für
  Größe und Zahl) → Umbenennen → Policy-Block → Nachweis. Ein Fehler nach dem Umbenennen rollt
  das Verzeichnis zurück.
- **Vertrauen per TOFU mit Bestätigung.** Beim ersten Paket eines Herausgebers zeigt `install`
  Namen und Fingerprint (erste 16 Hex von SHA-256 des Schlüssels) und fragt einmal; danach liegt
  der Schlüssel unter `<state_root>/trusted-keys/<name>.json` (0700/0600), und jedes weitere Paket
  dieses Namens muss dazu passen — ein anderer Schlüssel ist ein Fehler, nie eine stille Ersetzung.
  Nicht-interaktiv: `--trust-key <fingerprint>`; die Rechte-Zustimmung `--yes`; Variablen
  `--var NAME=WERT`. Fehlt etwas, bricht `install` mit genau der Liste ab, der man zugestimmt hätte.
- **Markierte Policy-Blöcke** (`policy_edit::write_package_section`/`remove_package_section`):
  `# von sepp pkg: <name> <version> — nicht von Hand ändern` … `# Ende sepp pkg: <name>`. Die
  Marker sind Kommentare, keine Schlüssel — jeder Metadaten-Schlüssel würde vom Loader als
  „unbekannt, ohne Wirkung" gemeldet. Ein Block wird als Zeilenbereich behandelt, weil `toml_edit`
  einen Kommentar *nach* dem letzten Wert nicht halten kann; das Ergebnis wird vor dem Schreiben
  noch einmal wie vom Loader geparst. Ein Upgrade ersetzt den Block an Ort und Stelle, `remove`
  stellt die Datei byte-identisch her, ein handgeschriebener Abschnitt gleichen Namens ist ein
  Fehler. Pfadrechte werden bei der Installation **absolut** geschrieben (`${BELEGE_DIR}` →
  `/home/anna/belege`), weil relative Pfade sonst gegen das Arbeitsverzeichnis des Prozesses
  aufgelöst würden; `sepp policy` zeigt echte Pfade.
- **Die Loader lesen `<config_root>/pkg/<name>/`** als weitere Wurzel — nach der globalen, vor der
  projektlokalen: Bei gleichnamigen Prompts gewinnt der Nutzer, Paket-Hooks laufen nach seinen.
  `settings.toml` bekommt bewusst keine Paketpfade. `sepp init` legt `pkg/` an, im State-Root
  `pkg/` und `trusted-keys/` mit 0700 — auch ohne `--system`. `sepp policy` zeigt Paket-Plugins
  ohne Änderung.
- **Kollisionen werden vorab geprüft.** Ein Plugin gleichen Namens beim Nutzer oder in einem
  anderen Paket ist ein Fehler (beide teilten sich die `[plugin.<name>]`-Gewährung); gleichnamige
  Prompts und Skills sind eine Warnung im Zustimmungsdialog. Ein Paketname gehört einem
  Herausgeber: Ein anderer darf ein installiertes Paket nicht überschreiben.
- **`[rights]` ⊆ Plugin-Manifest.** Ein Paket darf nur um die *Art* von Zugriff bitten, die das
  Plugin-Manifest deklariert (Host, Variable, Dateizugriff) — sonst wäre die Gewährung wirkungslos
  oder das Paket lügt. Ein Pfad außerhalb des Manifest-Präfixes ist eine Warnung („der Schnitt
  wäre leer"), kein Fehler. Paket-Hooks bekommen eine eigene Zustimmungszeile: Sie laufen im
  Agent-Loop und können jeden Werkzeugaufruf blockieren.
- **`pack` ist reproduzierbar** (sortierte Einträge, Modus 0644, mtime 0, uid/gid 0): zweimal
  packen ergibt dieselben Bytes. `pack` prüft wie der Installer, trägt `[files]` und den Public
  Key kommentarerhaltend ein und schreibt nie über ein vorhandenes Paket.

### Geändert
- **`policy_edit::allow` schreibt atomar** (temporäre Datei daneben, `fsync`, `rename`; neues
  `sepp_policy::fsutil`) und behält den Dateimodus — eine `policy.toml` unter `/etc/sepp` bleibt
  für alle lesbar. Die Doktrin des Moduls („nur ergänzen, nie entfernen") hat jetzt genau eine
  benannte Ausnahme: Blöcke, die `sepp pkg` selbst markiert hat.
- Die Krypto kommt aus `ring` (SHA-256, Ed25519, `SystemRandom`), das über `rustls` ohnehin im
  Baum liegt — keine neue Krypto-Dependency. Neu sind `tar`, `zstd` (ohne Default-Features) und
  `semver`; `toml` ist in `sepp-cli` jetzt eine echte Dependency.
- Dreizehn Crates im Gleichschritt (`sepp-pkg` ist neu).

### Tests
- `policy_edit`: Paketblock schreiben → parsen → erwartete Gewährungen; Upgrade ersetzt an Ort;
  `remove` stellt Bytes her; handgeschriebener Akteur und kaputte Marker werden abgelehnt;
  `allow` hinterlässt keine temporäre Datei und behält den Modus. `fsutil`: atomar, Modus,
  Symlink-Ziel, 0700.
- `sepp-pkg`: Manifest (Beispiel, unbekannte Felder, zwölf Ablehnungsfälle), Signatur
  (Round-Trip, manipuliert, falscher Schlüssel), Schlüsseldateien nie überschreiben; Container
  (Round-Trip und Reproduzierbarkeit, Hash-Mismatch, ungelistete und fehlende Datei, `..` und
  absolute Pfade, Symlink-Eintrag, übergroßer Header vor dem Lesen, falsches Magic, Manifest
  nicht zuerst); Variablen (Vorrang, Fehlende, Auflösung zu absoluten Pfaden); TOFU (neu →
  bekannt → anderer Schlüssel); Integration über den ganzen Weg (installieren, upgraden mit
  übernommenen Variablen, niedrigere Version ablehnen, entfernen, zweiter Herausgeber,
  Kollisionen, Rechte über das Manifest hinaus, manipuliertes Paket).
- `sepp-cli`: Parser, Datum ohne Zeitbibliothek, `init` legt `pkg/` und `trusted-keys/` (0700)
  an; `#[ignore]`-Test packt das gebaute `textstat.wasm`, installiert es gegen Wurzeln im
  Temp-Verzeichnis, lädt es aus `pkg/demo/plugins` und zählt Wörter.
- Pfad-Erwartungen in den Paket-Tests gehen vom kanonischen Temp-Verzeichnis aus statt von
  festen Host-Pfaden — auf macOS liegt `TMPDIR` unter `/var` → `/private/var` und `/home` ist
  ein Symlink, was den macOS-Lauf der CI rot gemacht hatte.

## [0.3.0] - 2026-09-06

Der Autor eines Plugins schreibt eine Funktion, kein Protokoll. Das Beispiel-Plugin hatte 163
Zeilen, davon rund 107 Zeiger, Exports und JSON-Hüllen — und genau 13 Zeilen Arbeit. Das ist der
Grund, warum es keine Fremdplugins gab: nicht mangelndes Interesse, sondern ein Aufrufprotokoll,
das man erst abschreiben muss. Ab jetzt kapselt ein SDK das Protokoll, ein Attribut macht aus der
Funktion das Werkzeug, und die Schnittstelle steht als Vertrag in einer Datei, gegen die ein Test
den Host prüft. Alles davon ist **additiv**: Das ABI bleibt bei Version 1, ein bestehendes Modul
merkt nichts.

Und `host_http` ist keine Attrappe mehr. Ein Plugin hat keine Sockets, nur diese eine Funktion —
sepp **ist** der Netzwerkstack des Moduls. Deshalb kann der Host an der Grenze durchsetzen, was für
`bash` und MCP-Kindprozesse erst der Egress-Proxy bringt: die Host-Allowlist exakt je Anfrage,
Secrets erst im Host eingesetzt, jede Anfrage in der Audit-Spur. Der Satz aus dem Paket-Plan gilt
damit strukturell: Das Modul kennt deinen Schlüssel nicht. Es kann ihn nicht kennen.

### Hinzugefügt
- **`host_http` als durchsetzender Proxy.** Je Anfrage prüft der Host in dieser Reihenfolge und
  fail-closed: Zähler (`max_http_requests` je Werkzeugaufruf) → http(s)-URL → keine
  `$NAME`-Platzhalter in der URL (sie stünde in jeder Fehlermeldung) → der Host der URL ist
  gewährt (exakt, `*.suffix` oder `*` — dieselbe `Policy::allows`, die überall gilt) → je
  Platzhalter in einem Header-Wert das **Doppel-Gate** wie bei http-MCP-Servern (Host gewährt,
  Variable per `env` gewährt, Variable gesetzt), erst dann wird ersetzt → Body und Methode →
  Abbruch und Zeitbudget (`http_timeout_ms`, auf die Rest-Wanduhr gekappt) → Anfrage. Vor der
  Allowlist geht kein Byte auf die Leitung; jeder Fehler nennt den passenden `sepp policy allow
  plugin.<name> …`-Befehl und nie einen Wert. **Keine automatischen Redirects**: Ein 3xx auf einen
  anderen Host wäre eine Anfrage, die die Allowlist nie gesehen hat — es geht als Antwort ans
  Modul, das selbst neu anfragen darf, und jeder Hop läuft durch dieselbe Prüfung. Die Antwort
  wird **gestreamt und vor dem Puffern gedeckelt** (`max_http_response_bytes`: Content-Length
  vorab, beim Lesen gezählt). Die Ausführung liegt in einem eigenen Thread `sepp-http` mit eigener
  Runtime, weil die Host-Funktion synchron in der wasmi-Closure läuft — beim Werkzeugaufruf im
  Blocking-Pool, beim Laden auf dem Reactor-Thread, wo `Handle::block_on` panicken würde. Der
  Thread startet beim ersten Auftrag, bedient parallele Aufrufe nebenläufig, wartet je Anfrage mit
  `select!` auf Antwort, Abbruch oder Timeout und endet mit dem Host. Textantworten bleiben Text;
  was kein UTF-8 ist, kommt als `body_base64` (Anfragen entsprechend mit `body_base64`).
- **Die Audit-Spur kennt HTTP.** Jeder Versuch eines Plugins — auch ein abgelehnter — landet als
  Eintrag `kind = "plugin_http"` in der Sitzung (`details.audit` des Werkzeugergebnisses, ein
  Objekt je Aufruf mit allen Anfragen: Methode, Host, URL ohne Query, Status oder Fehler, Bytes,
  Dauer, **Namen** ersetzter Secrets, nie Werte). `sepp audit` zeigt je Anfrage eine Zeile
  (`HTTP  datev · GET api.example.com/belege/1 · 200 · 1,2 KB · 83 ms`, abgelehnte als `DENY …`)
  und zählt Verweigerungen in der Fußzeile mit; `/tree` blendet die Einträge wie `guard` aus.
  Der Schlüssel `audit` in `details` gehört dem Host — ein Test in `sepp-cli` hält ihn mit
  `sepp_agent::AUDIT_DETAIL_KEY` gleich, weil sich die beiden Crates nicht kennen.
- **Neue `[limits]`-Felder im Manifest**, additiv mit Defaults: `max_http_requests` (16),
  `max_http_response_bytes` (4 MiB, höchstens 16 MiB — der Ergebnisdeckel des Hosts, ein Test hält
  beide gleich), `http_timeout_ms` (10 000). Die Gerüst-Vorlage von `sepp plugin new` nennt sie
  auskommentiert und erklärt `env = [...]`.
- **SDK: Binär-Bodies.** `RequestBuilder::body_bytes`, `Response::bytes()`/`text()`/`header()`/
  `is_success()`; `base64` kommt nur mit dem Feature `net` ins Modul — ein Plugin ohne Netz wächst
  nicht. `sepp policy` nennt für Plugins jetzt die echten Vollstrecker („Host-Allowlist je Anfrage
  (host_http); Secrets: Broker", „Pfadprüfung je Aufruf (host_fs_read)") statt „Stub", und die
  Zeile „Host-Filter nicht durchsetzbar" gilt nur noch für `agent`/`mcp` — für Plugins ist er es.
- **Guest-SDK `sepp-plugin` und Attribut `#[sepp_plugin::tool]`** (`sepp-plugin-macros`). Aus
  `fn name(args: Args, host: &Host) -> Result<ToolResult>` werden Exports, Zeigerarithmetik,
  Abholweg und Fehlerhülle erzeugt; das Parameter-Schema entsteht aus `Args` (`schemars`), ein
  `Err` wird zum Ergebnis mit `is_error = true`. Fähigkeiten sind **Cargo-Features und damit
  Compile-Gates**: `host.fs()` nur mit `fs-read`, `host.http()` nur mit `net` — ein Feature
  schaltet zugleich den Host-Import frei, das Modul importiert nur, was es benutzt, und das
  Linker-Gate des Hosts bleibt konsistent. Dateien liest das SDK roh (`host_fs_read_bytes`), ein
  PDF kommt als PDF an. Das SDK kompiliert auch nativ (Exports nur unter `wasm32`, Fähigkeiten
  liefern nativ einen Fehler), sodass ein Autor sein Werkzeug mit `cargo test` ohne wasm32-Target
  prüft und die Crates durch Clippy, Tests und CI laufen. Ein `#[tool]` je Crate — ABI 1 kennt ein
  Werkzeug je Modul; ein zweites ist ein Compiler-Fehler. Die Exports entstehen im Crate des
  Autors, nicht im SDK: Der Export von `#[no_mangle]`-Symbolen aus einer Bibliothek in die
  `cdylib` ist ein Implementierungsdetail von rustc, kein Vertrag.
- **`wit/sepp.wit` als Vertragstext der Plugin-Schnittstelle.** Die logische Schnittstelle als
  WIT (`fs-read` typisiert als `result<list<u8>, string>` — genau daran war der Lossy-Fehler
  aufgefallen; `http` mit Request-/Response-Records) und darunter die Kodierung für Core-WASM. Kein Generator, kein Component Model: WIT ist Bauzeit, wasmi bleibt
  Laufzeit. Der Host trägt seine Importe jetzt in einer Tabelle (`HOST_IMPORTS` mit `Gate`,
  `EXPORTS`), aus der `build_linker` und `check_exports` lesen, und ein Test hält Tabelle und
  WIT synchron — eine Funktion, die die eine Seite kennt und die andere nicht, fällt sofort auf.
- **`sepp plugin new <name>`** legt ein Plugin-Gerüst an (`Cargo.toml`, `src/lib.rs` mit
  nativem Test, `<name>.toml`, `README.md`): Der Name ist zugleich Paket-, Datei-, Funktions- und
  Werkzeugname, deshalb enger geprüft als beim Host. Die SDK-Dependency zeigt per Git-Tag auf die
  Version des laufenden `sepp` (die Crates liegen nicht auf crates.io); `--sdk-path` schreibt
  stattdessen eine `path`-Dependency für die Entwicklung. Das Manifest nennt beim Wanduhr-Budget
  den Fehler, den man einmal macht: an einer zweiseitigen Rechnung testen, 5000 ms setzen, und der
  erste 90-seitige Sammelbeleg bricht ab.
- **`schema_for` in `sepp-core`** (Feature `schema`), damit die eingebauten Tools und das SDK
  dasselbe bereinigte Schema erzeugen; `sepp_tools::schema_for` bleibt als Re-Export.

### Geändert
- **Das Beispiel-Plugin `textstat` ist mit dem SDK geschrieben**: 44 Zeilen inklusive Doku statt
  163, nichts davon Protokoll; Ausgabe und Verhalten unverändert (der `#[ignore]`-Test
  `example_plugin_builds_and_runs` prüft das). Das Modul wächst von 70 KB auf 106 KB, weil das
  Schema zur Laufzeit aus dem Typ entsteht — Ladezeit einmalig beim Start, kein Thema je Aufruf.
  Die README des Beispiels erklärt jetzt den SDK-Weg zuerst und nennt korrekt **fünf**
  Host-Importe (`host_fs_read_bytes` fehlte seit 0.2.1).
- `sepp-core` zieht optional `schemars` (Feature `schema`); bleibt I/O-frei und synchron.
- **`url_host` und der Kern des Doppel-Gates liegen in `sepp-policy`** (`SecretBroker::gate`,
  `from_env_for`, `GateRefusal::explain`, `Actor::cli_name`), damit MCP-Client und WASM-Host
  dieselbe Kette prüfen und dieselben Sätze sagen; `sepp_mcp::url_host` bleibt als Re-Export, die
  MCP-Meldungen sind wortgleich.
- `sepp-wasm` zieht `reqwest` (Workspace-Version, Redirects aus, Connect-Timeout, User-Agent
  `sepp/<version>`) und `base64`; `WasmPlugin` hält seinen unveränderlichen Kern in einem `Arc`,
  und der Abschnittsname `[plugin.<name>]` wird getrennt vom exponierten Werkzeugnamen geführt —
  `rename` (Kollisionspräfix `wasm__`) verfälscht so keinen `sepp policy allow`-Hinweis mehr.

### Tests
- `host_http` gegen lokale Listener: erlaubter Host geht auf die Leitung (User-Agent, Status,
  Body, Header, Audit); nicht gewährter Host → kein Connect, Fehlertext nennt den Befehl;
  Secret-Header nur mit beiden Gewährungen auf der Leitung, Wert in keinem Text, Name in der Spur;
  ohne `env`-Gewährung geht nichts raus; 302 wird übergeben, nicht gefolgt; binäre Antwort als
  base64; Übergröße, stummer Server (Timeout in der Zeit), Abbruch während einer hängenden
  Anfrage → `Aborted`; Regelkette ohne Modul (URL-Platzhalter, Schema, Zähler, Deadline,
  Redaktion, Audit-Bündelung); Worker allein (lazy Start, Deckel vor und beim Lesen, Timeout,
  Cancel, kein Redirect, abgelehnte Verbindung ohne Panik). `#[ignore]`-Test: ein Gerüst von
  `sepp plugin new` mit Feature `net` baut für wasm32, lädt mit Gewährung und spricht über
  `host_http` mit einem lokalen Listener.
- Sync-Test WIT ↔ `HOST_IMPORTS`/`EXPORTS`; unter voller Gewährung lädt ein Modul mit allen
  Importen, ohne Gewährung fehlt jeder gegatete Import einzeln. Makro-Fehlerpfade gegen `expand`
  (fehlendes `desc`, ungültiger Name, falsche Signatur, `async`, Generics); Makro-Erfolgspfad
  nativ über `__sepp_plugin_export`; SDK-Kodierung (`pack` mit hohem Bit, Vorzeichen-Konvention
  inklusive `i32::MIN`, Fehlerobjekte). Gerüst: Parser, Namensregel, Vorlagen ohne Platzhalter,
  Manifest parst ohne unbekannte Schlüssel, nie überschreiben; `#[ignore]`-Test baut und testet
  das Gerüst nativ, baut es für wasm32 und lädt es im Host.

## [0.2.1] - 2026-09-05

Zwei Vorarbeiten für Plugin-Pakete, die aber unabhängig davon nützlich sind — und eine davon
repariert einen Fehler, der jeden Turn lahmlegen kann. Deshalb kommen sie sofort und warten
nicht auf das Paketformat.

Beides ist **rein additiv**: Das Plugin-ABI bleibt bei Version 1, und ein bestehendes Modul,
das `host_fs_read_bytes` nicht importiert, merkt von der Änderung nichts.

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

### Behoben
- **Tool-Namen werden gegen `^[A-Za-z0-9_-]{1,64}$` geprüft.** Anthropic und OpenAI lehnen alles
  andere mit `400` ab — und zwar den **ganzen** Request, nicht nur das eine Werkzeug. Ein
  einziger Doppelpunkt aus einer fremden Quelle legte damit jeden Turn lahm, bis man den Server
  abklemmte. Je nach Quelle wird unterschiedlich reagiert: Bei **MCP** gehört der Name dem
  fremden Server und wird für die Anzeige **saniert** (aufgerufen wird weiterhin der
  Originalname), inklusive Längengrenze auch nach Präfix und Zähl-Suffix. Bei **WASM** gehört er
  dem Plugin-Autor und wird beim Laden **abgelehnt** — ein stillschweigend umbenanntes Werkzeug
  wäre schlimmer als ein klarer Ladefehler, denn das Plugin beschreibt sich ja unter diesem Namen.

### Tests
- `host_fs_read_bytes` liefert rohe Bytes statt verlustbehaftetem Text (zwei ungültige
  UTF-8-Bytes: roh 2, lossy wären 6 — die Zahl unterscheidet beide Wege eindeutig); ein
  verweigerter Zugriff meldet sich negativ; die gemeinsame Prüfhälfte `read_granted_file` gegen
  Policy, Eingabefehler und byte-identische Rückgabe.
- Ein Plugin mit unzulässigem Werkzeugnamen lädt nicht; `resolve_name` liefert für jede fremde
  Eingabe einen anbieter-gültigen Namen und bleibt auch mit langem Präfix und Suffix unter 64.

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

[Unreleased]: https://github.com/Vezir0013/sepp-mini/compare/v0.5.2...HEAD
[0.5.2]: https://github.com/Vezir0013/sepp-mini/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/Vezir0013/sepp-mini/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/Vezir0013/sepp-mini/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/Vezir0013/sepp-mini/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/Vezir0013/sepp-mini/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/Vezir0013/sepp-mini/compare/v0.2.0...v0.2.1
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
