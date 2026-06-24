<!-- Danke für deinen Beitrag! Bitte halte den PR fokussiert (eine logische Einheit). -->

## Was & Warum

<!-- Kurz: Was ändert dieser PR und warum? -->

## Art der Änderung

- [ ] `fix` — Bugfix
- [ ] `feat` — neue Funktion
- [ ] `refactor` / `perf`
- [ ] `docs`
- [ ] `test` / `chore` / `ci`

## Checkliste

- [ ] `just check` ist grün (fmt + clippy `-D warnings` + tests)
- [ ] Tests mit der Implementierung geschrieben/angepasst
- [ ] Öffentliche Items mit `///` dokumentiert
- [ ] Keine `unwrap`/`expect`/`panic` in Library-Crates; keine Secrets im Diff
- [ ] Bei Format-/Protokoll-Änderungen: nur additiv (oder Migrationsplan beschrieben)
- [ ] `CHANGELOG.md` aktualisiert (falls nutzersichtbar)
