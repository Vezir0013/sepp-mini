//! Gemeinsamer HTTP-Unterbau der Adapter: Client mit Verbindungs-Timeout, abbrechbares Senden,
//! gedeckelte Fehler-Bodies und der Wiederanlauf bei vorübergehenden Störungen des Anbieters.
//!
//! **Warum hier und nicht je Adapter.** Es gibt zwei Request-Pfade — [`crate::anthropic`] und
//! [`crate::openai::stream_chat`], letzterer bedient auch Moonshot und z.ai —, aber fünf Stellen,
//! an denen ein `reqwest::Client` entsteht. Timeouts, Cancel-Verhalten und Wiederanlauf gehören
//! an eine Stelle, sonst driften sie auseinander.
//!
//! **Die drei Zusagen dieses Moduls:**
//!
//! 1. *Der Verbindungsaufbau ist begrenzt* ([`CONNECT_TIMEOUT`]). Ohne ihn hängt ein Aufruf an
//!    einen toten Endpunkt (falsche `OPENAI_BASE_URL`, LM Studio nicht gestartet) am
//!    OS-Timeout, das Minuten dauern kann. Ein **Lese**-Timeout gibt es bewusst nicht:
//!    Reasoning-Modelle schweigen vor dem ersten Token teils minutenlang, und ein Deckel darauf
//!    würde genau die langen, teuren Antworten abschneiden.
//! 2. *Jede Wartephase ist abbrechbar.* Der `CancellationToken` wurde bisher erst im laufenden
//!    Stream beachtet; `send()` und das Lesen des Fehler-Bodys waren blind dafür. Alle
//!    `select!` hier sind `biased`, damit ein gesetzter Token gewinnt und nicht gegen einen
//!    gleichzeitig fertigen Chunk auslost.
//! 3. *Nichts wird unbegrenzt gepuffert.* Der Fehler-Body ist auf [`MAX_ERROR_BODY`] gedeckelt.
//!
//! **Wiederanlauf, und wo seine Grenze liegt.** [`send_with_retry`] wiederholt nur, solange
//! **kein Byte des Streams geflossen ist**: `send()` kehrt mit den Kopfzeilen zurück, der Body
//! streamt danach. Ein Abbruch mitten im Stream wird nie wiederholt — der Nutzer sähe sonst
//! Text doppelt. Wiederholt wird ausschließlich nach einer *Antwort* des Servers
//! ([`is_retryable`]); ein Transportfehler bedeutet, dass die Adresse falsch oder der Dienst aus
//! ist, und ein zweiter Versuch kostet dort nur Zeit.

use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use sepp_core::SeppError;

/// Wartezeit auf den Verbindungsaufbau (TCP + TLS). Kein Lese- und kein Gesamt-Timeout —
/// siehe Modul-Doku.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Obergrenze für den Fehler-Body, der in eine Meldung wandert. Ein Anbieter, der auf einen
/// abgelehnten Request eine Megabyte-HTML-Seite antwortet, soll weder den Speicher noch das
/// Terminal noch (über die Fehlermeldung) das Kontextfenster fluten.
pub(crate) const MAX_ERROR_BODY: usize = 64 * 1024;

/// Ein fehlgeschlagener Request. `status` ist gesetzt, wenn der Server geantwortet hat — nur
/// dann lässt sich über einen Wiederanlauf entscheiden. Bei Transport- und Abbruchfehlern ist
/// es `None`.
#[derive(Debug)]
pub(crate) struct HttpFail {
    pub(crate) status: Option<reqwest::StatusCode>,
    pub(crate) error: SeppError,
}

impl HttpFail {
    /// Ein Fehler ohne Antwort des Servers (Verbindung, DNS, TLS, Abbruch).
    pub(crate) fn transport(error: SeppError) -> Self {
        HttpFail {
            status: None,
            error,
        }
    }

    /// Gibt den zugrunde liegenden Fehler heraus; der Status ist nur für die Retry-Entscheidung
    /// und den `reasoning_effort`-Fallback interessant.
    pub(crate) fn into_sepp(self) -> SeppError {
        self.error
    }
}

/// Wie oft und wie lange wiederholt wird.
///
/// Die Voreinstellung ist bewusst knapp: Drei Versuche mit zusammen höchstens einigen Sekunden
/// überbrücken die typische Lastspitze eines Anbieters, ohne dass ein Mensch vor einem
/// scheinbar hängenden Programm sitzt. Wer länger warten will, wiederholt den Prompt.
#[derive(Debug, Clone)]
pub(crate) struct RetryPolicy {
    /// Gesamtzahl der Versuche, den ersten eingeschlossen.
    pub(crate) attempts: u32,
    /// Grundwartezeit; sie verdoppelt sich je Versuch.
    pub(crate) base: Duration,
    /// Deckel für eine einzelne Wartezeit — auch für ein `Retry-After` des Servers.
    pub(crate) max_wait: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            attempts: 3,
            base: Duration::from_secs(1),
            max_wait: Duration::from_secs(30),
        }
    }
}

/// Eine erfolgreiche Antwort samt der Hinweise, die auf dem Weg dorthin entstanden sind.
///
/// Die Hinweise werden dem Stream vorangestellt (als [`crate::StreamEvent::Notice`]), weil der
/// `Provider`-Trait keinen zweiten Kanal nach oben hat. Sie erklären dem Menschen die
/// Verzögerung, die er gerade erlebt hat.
pub(crate) struct SendOutcome {
    pub(crate) response: reqwest::Response,
    pub(crate) notices: Vec<String>,
}

/// Baut den HTTP-Client aller Adapter.
///
/// Fällt auf `Client::new()` zurück, wenn der Builder scheitert (praktisch nur bei kaputter
/// TLS-Initialisierung). Das ist strikt besser als ein `unwrap`: `Client::new()` tut intern
/// dasselbe, nur mit Panik statt Rückfall — und Library-Crates dürfen hier nicht panicken.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(concat!("sepp/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Lohnt sich ein weiterer Versuch bei diesem Status?
///
/// `429` (Ratenlimit) und `408` (der Server hat zu lange auf uns gewartet) sind die beiden
/// 4xx, die vorübergehend sind; alles andere unterhalb von 500 ist ein Konfigurations- oder
/// Anfragefehler, den ein zweiter Versuch nur wiederholen würde. Ab `500` ist die Ursache
/// beim Anbieter — `529` („overloaded") schickt Anthropic unter Last regelmäßig.
pub(crate) fn is_retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status.as_u16() >= 500
}

/// Kurze deutsche Benennung des Grundes für den Hinweis an den Menschen.
fn reason_of(status: reqwest::StatusCode) -> &'static str {
    match status.as_u16() {
        429 => "Ratenlimit",
        408 => "Zeitüberschreitung",
        503 | 529 => "überlastet",
        _ => "Serverfehler",
    }
}

/// Wandelt einen `Retry-After`-Kopfzeilenwert in eine Wartezeit.
///
/// RFC 9110 erlaubt zwei Formen: eine Zahl in Sekunden oder ein HTTP-Datum. `now_unix` wird
/// übergeben statt gelesen, damit die Funktion rein und ohne Uhr testbar bleibt. Ein Datum in
/// der Vergangenheit ergibt `Some(0)` (sofort erlaubt), unlesbares ergibt `None`.
pub(crate) fn parse_retry_after(value: &str, now_unix: i64) -> Option<Duration> {
    let v = value.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = parse_http_date(v)?;
    Some(Duration::from_secs((at - now_unix).max(0) as u64))
}

/// Parst ein IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) zu Unix-Sekunden.
///
/// Nur diese eine Form: Sie ist laut RFC 9110 die einzige, die ein Server *senden* darf; die
/// beiden veralteten Formen zu lesen wäre Aufwand für einen Fall, der bei den unterstützten
/// Anbietern nicht vorkommt. Scheitert das Parsen, greift der normale Backoff.
fn parse_http_date(s: &str) -> Option<i64> {
    // "Sun, 06 Nov 1994 08:49:37 GMT" — Wochentag und Zeitzone werden nicht ausgewertet:
    // das Format schreibt GMT fest, und der Wochentag ist redundant.
    let rest = s.split_once(", ")?.1;
    let mut parts = rest.split(' ');
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let m: i64 = hms.next()?.parse().ok()?;
    let sec: i64 = hms.next()?.parse().ok()?;
    if !(1..=31).contains(&day) || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + h * 3600 + m * 60 + sec)
}

/// Tage seit 1970-01-01 für ein gregorianisches Datum (Algorithmus nach Howard Hinnant).
///
/// Eine eigene Zeile Kalenderarithmetik statt einer Datums-Abhängigkeit: Der Workspace pinnt
/// jede Version exakt, und eine Crate für dreißig Zeilen wäre der teurere Weg.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Wartezeit vor dem nächsten Versuch.
///
/// `attempt` ist der gerade gescheiterte Versuch (1-basiert). Ein `Retry-After` des Servers
/// schlägt den eigenen Backoff — er weiß besser, wann er wieder kann. Beides wird auf
/// `policy.max_wait` gedeckelt: Eine Stunde stillzustehen ist von einem Hänger nicht zu
/// unterscheiden, dann lieber ein ehrlicher Fehler.
///
/// `jitter_permille` (0..1000) verteilt gleichzeitig gestartete Prozesse, damit sie nicht im
/// Gleichschritt erneut anklopfen; als Parameter, damit die Funktion rein bleibt.
pub(crate) fn retry_delay(
    attempt: u32,
    retry_after: Option<Duration>,
    policy: &RetryPolicy,
    jitter_permille: u32,
) -> Duration {
    if let Some(d) = retry_after {
        return d.min(policy.max_wait);
    }
    let factor = 1u32 << attempt.saturating_sub(1).min(16);
    let base = policy.base.saturating_mul(factor);
    // Bis zu einem Viertel der Grundwartezeit obendrauf.
    let jitter = policy
        .base
        .mul_f64(0.25 * f64::from(jitter_permille.min(1000)) / 1000.0);
    base.saturating_add(jitter).min(policy.max_wait)
}

/// Zufallsanteil aus der Systemuhr — ohne `rand`-Abhängigkeit, gut genug, um gleichzeitig
/// gestartete Prozesse zu entzerren.
fn clock_jitter() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() % 1000)
        .unwrap_or(0)
}

/// Aktuelle Unix-Zeit in Sekunden (für `Retry-After` als Datum).
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Schickt einen Request und bricht ab, sobald `cancel` gesetzt ist.
///
/// `biased`, damit ein bereits gesetzter Token gewinnt: Ohne das würfelt `select!` zwischen
/// Abbruch und einer gleichzeitig fertigen Antwort.
async fn send_cancelable(
    builder: reqwest::RequestBuilder,
    label: &str,
    base_url: &str,
    cancel: &CancellationToken,
) -> std::result::Result<reqwest::Response, HttpFail> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(HttpFail::transport(SeppError::Aborted)),
        r = builder.send() => r.map_err(|e| {
            // Verbindungsfehler nennen den Endpunkt: hilfreicher bei lokalen Servern
            // (mlx/local), die zwischen Preflight und Request sterben können, statt eines
            // rohen reqwest-Texts.
            if e.is_connect() {
                HttpFail::transport(SeppError::Provider(format!(
                    "{label}: Verbindung zu {base_url} fehlgeschlagen: {e} — läuft der Server?"
                )))
            } else {
                HttpFail::transport(SeppError::Provider(format!("{label} request: {e}")))
            }
        }),
    }
}

/// Liest den Body einer Fehlerantwort, höchstens [`MAX_ERROR_BODY`] Bytes.
///
/// Streamt statt `text()`, damit ein riesiger Body gar nicht erst vollständig im Speicher
/// landet. Abbruch und Lesefehler liefern, was bis dahin da ist — die Statuszeile allein ist
/// schon eine brauchbare Meldung.
async fn error_body(resp: reqwest::Response, cancel: &CancellationToken) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            c = stream.next() => c,
        };
        match next {
            Some(Ok(chunk)) => {
                let room = MAX_ERROR_BODY.saturating_sub(buf.len());
                if room == 0 {
                    break;
                }
                let take = chunk.len().min(room);
                buf.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    break;
                }
            }
            _ => break,
        }
    }
    String::from_utf8_lossy(&buf).trim().to_string()
}

/// Schickt den Request und wiederholt ihn bei vorübergehenden Störungen des Anbieters.
///
/// `build` liefert je Versuch einen frischen `RequestBuilder` — ein `RequestBuilder` ist nach
/// `send()` verbraucht, deshalb eine Closure statt eines Werts.
///
/// Die Fehlermeldung behält das Format `<label>: HTTP <status>: <body>`; `sepp-agent` erkennt
/// daran einen zu langen Kontext (`compact::looks_like_context_overflow`). Der Versuchszähler
/// wird nur **angehängt**, damit dieser Präfix unverändert bleibt.
pub(crate) async fn send_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
    label: &str,
    base_url: &str,
    policy: &RetryPolicy,
    cancel: &CancellationToken,
) -> std::result::Result<SendOutcome, HttpFail> {
    let mut notices: Vec<String> = Vec::new();
    let total = policy.attempts.max(1);

    for attempt in 1..=total {
        let resp = send_cancelable(build(), label, base_url, cancel).await;
        let resp = match resp {
            Ok(r) => r,
            // Transportfehler werden nicht wiederholt: Die Adresse ist falsch oder der Dienst
            // ist aus. Das Verbindungs-Timeout begrenzt den Versuch ohnehin.
            Err(e) => return Err(e),
        };

        let status = resp.status();
        if status.is_success() {
            return Ok(SendOutcome {
                response: resp,
                notices,
            });
        }

        let last = attempt >= total;
        if last || !is_retryable(status) {
            let body = error_body(resp, cancel).await;
            let mut msg = format!("{label}: HTTP {status}: {body}");
            if attempt > 1 {
                msg.push_str(&format!(" (nach {attempt} Versuchen)"));
            }
            return Err(HttpFail {
                status: Some(status),
                error: SeppError::Provider(msg),
            });
        }

        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| parse_retry_after(v, now_unix()));
        // Body verwerfen, bevor die Verbindung zurückgeht — sonst bliebe sie belegt.
        drop(resp);

        let wait = retry_delay(attempt, retry_after, policy, clock_jitter());
        let note = format!(
            "{label}: {} ({}) — Versuch {} von {} in {}",
            reason_of(status),
            status.as_u16(),
            attempt + 1,
            total,
            human_duration(wait),
        );
        tracing::warn!("{note}");
        notices.push(note);

        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(HttpFail::transport(SeppError::Aborted)),
            _ = tokio::time::sleep(wait) => {}
        }
    }

    // Unerreichbar: Die Schleife kehrt in jedem Zweig zurück. Ein sprechender Fehler ist
    // trotzdem besser als ein `unreachable!()` in einem Library-Crate.
    Err(HttpFail::transport(SeppError::Provider(format!(
        "{label}: kein Versuch ausgeführt"
    ))))
}

/// Wartezeit für Menschen: „2 s" statt „2.000000001s".
fn human_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.0} s", d.as_secs_f64().round())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_covers_transient_only() {
        for code in [429u16, 408, 500, 502, 503, 529] {
            let s = reqwest::StatusCode::from_u16(code).expect("status");
            assert!(is_retryable(s), "{code} sollte wiederholbar sein");
        }
        for code in [400u16, 401, 403, 404, 422] {
            let s = reqwest::StatusCode::from_u16(code).expect("status");
            assert!(!is_retryable(s), "{code} darf nicht wiederholt werden");
        }
    }

    #[test]
    fn retry_after_reads_seconds_and_zero() {
        assert_eq!(parse_retry_after("3", 0), Some(Duration::from_secs(3)));
        assert_eq!(parse_retry_after(" 0 ", 0), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("morgen", 0), None);
        assert_eq!(parse_retry_after("-5", 0), None);
    }

    #[test]
    fn retry_after_reads_http_date() {
        // 1994-11-06 08:49:37 GMT = 784111777 (Referenzwert aus RFC 9110).
        let at = 784_111_777;
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", at - 60),
            Some(Duration::from_secs(60))
        );
        // Ein Datum in der Vergangenheit heißt „sofort", nicht „negativ".
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", at + 60),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("Sun, 06 Foo 1994 08:49:37 GMT", 0), None);
    }

    #[test]
    fn civil_days_match_known_epochs() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
    }

    #[test]
    fn delay_doubles_and_is_capped() {
        let p = RetryPolicy::default();
        // Ohne Jitter exakt die Verdopplung.
        assert_eq!(retry_delay(1, None, &p, 0), Duration::from_secs(1));
        assert_eq!(retry_delay(2, None, &p, 0), Duration::from_secs(2));
        assert_eq!(retry_delay(3, None, &p, 0), Duration::from_secs(4));
        // Der Deckel greift auch bei absurd vielen Versuchen.
        assert_eq!(retry_delay(20, None, &p, 0), p.max_wait);
    }

    #[test]
    fn jitter_adds_at_most_a_quarter_of_base() {
        let p = RetryPolicy::default();
        let d = retry_delay(1, None, &p, 1000);
        assert_eq!(d, Duration::from_millis(1250));
        assert!(retry_delay(1, None, &p, 500) < d);
    }

    #[test]
    fn server_retry_after_wins_but_stays_capped() {
        let p = RetryPolicy::default();
        // Der Server darf früher zurückrufen, als der eigene Backoff vorsähe …
        assert_eq!(
            retry_delay(3, Some(Duration::from_secs(1)), &p, 999),
            Duration::from_secs(1)
        );
        // … aber eine Stunde Stillstand ist von einem Hänger nicht zu unterscheiden.
        assert_eq!(
            retry_delay(1, Some(Duration::from_secs(3600)), &p, 0),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn durations_are_readable() {
        assert_eq!(human_duration(Duration::from_millis(250)), "250 ms");
        assert_eq!(human_duration(Duration::from_secs(2)), "2 s");
    }
}
