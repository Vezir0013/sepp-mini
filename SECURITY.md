# Sicherheitsrichtlinie

`sepp mini` ist ein **sicherheitsorientierter** Agent-Harness (sandbox-by-default, default deny).
Wir nehmen Schwachstellen ernst.

## Schwachstellen melden

Bitte **keine** öffentlichen Issues für Sicherheitslücken eröffnen.

- Bevorzugt: GitHub → Reiter **„Security" → „Report a vulnerability"** (privates
  Security-Advisory).
- Alternativ per E-Mail: **a.herco@13pm.network** (gerne PGP — Key auf Anfrage).

Bitte beschreibe möglichst genau: betroffene Version/Commit, Reproduktionsschritte, erwartetes
vs. tatsächliches Verhalten und die mögliche Auswirkung. Wir bestätigen den Eingang zeitnah und
halten dich über den Fortschritt auf dem Laufenden. Verantwortungsvolle Offenlegung wird
gewürdigt.

## Unterstützte Versionen

Das Projekt ist in aktiver Entwicklung (v0.x). Sicherheitsfixes fließen in den `main`-Branch und
das jeweils jüngste Release ein.

| Version | Unterstützt |
|---------|-------------|
| 0.1.x   | ✅          |
| < 0.1   | ❌          |

## Bedrohungsmodell (Kurzfassung)

Abgewehrt wird vor allem eine **bösartige oder schlampige Erweiterung** (Hook, WASM-Plugin,
MCP-Server), die geheime Dateien liest, API-Keys exfiltriert oder nach Hause telefoniert, sowie
**Prompt-Injection** über Tool-Ausgaben. Durchsetzung: Default-deny-Capabilities, OS-Sandbox
(Linux: Landlock, macOS: Seatbelt) für Subprozesse, Environment-Scrubbing, capability-gegatete
WASM-Host-Funktionen, keine Secrets im Klartext an Erweiterungen oder Logs.

**Nicht** im Modell: ein bösartiger Kern selbst, Kernel-0days, physischer Zugriff.

## Bekannte Einschränkungen (v0.1)

- Die OS-Sandbox (Landlock/Seatbelt) begrenzt aktuell nur das **Dateisystem**; eine Netz-Sandbox
  für MCP-Subprozesse (seccomp/Namespaces) ist geplant.
- Dateisystem-Sandboxing gibt es auf **Linux** (Landlock) und **macOS** (Seatbelt); lässt es sich
  nicht durchsetzen, wird fail-closed verfahren.
- Auf Plattformen ohne Adapter (Windows/BSD) gibt es **kein** OS-FS-Sandboxing — nur
  Environment-Scrubbing, mit deutlicher Warnung.
