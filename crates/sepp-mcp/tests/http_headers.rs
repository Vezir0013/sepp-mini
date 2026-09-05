//! Beweist die Verdrahtung: Landet der aufgelöste Secret-Header wirklich auf der Leitung —
//! und schweigt der Client, wenn die Policy das Ziel nicht gewährt?
//!
//! Ein echter TCP-Listener statt eines Mocks: Alle reinen Gate-Tests stehen in `src/lib.rs`,
//! aber ob rmcp die `custom_headers` auch sendet, sagt nur der Bytestrom.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use sepp_mcp::{connect_with_policy, McpServerConfig};
use sepp_policy::{Capability, Policy};

/// Startet einen Listener, der den ersten Request-Head einsammelt und 500 antwortet.
/// Liefert die URL und den geteilten Puffer.
async fn spy() -> (String, Arc<Mutex<Vec<u8>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            if let Ok(n) = sock.read(&mut buf).await {
                sink.lock().await.extend_from_slice(&buf[..n]);
            }
            let _ = sock
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                .await;
        }
    });
    (format!("http://{addr}/mcp"), seen)
}

fn cfg(url: &str, headers: BTreeMap<String, String>) -> McpServerConfig {
    McpServerConfig {
        name: "spy".into(),
        transport: "http".into(),
        url: Some(url.to_string()),
        command: vec![],
        capabilities: Default::default(),
        headers,
    }
}

#[tokio::test]
async fn resolved_secret_header_reaches_the_wire() {
    std::env::set_var("SEPP_TEST_WIRE_TOKEN", "sk-auf-der-leitung");
    let (url, seen) = spy().await;
    let host = "127.0.0.1";
    let mut headers = BTreeMap::new();
    headers.insert(
        "authorization".to_string(),
        "Bearer $SEPP_TEST_WIRE_TOKEN".to_string(),
    );
    let policy = Policy::new(vec![
        Capability::Net {
            host: host.to_string(),
        },
        Capability::Env {
            name: "SEPP_TEST_WIRE_TOKEN".into(),
        },
    ]);

    // Der Connect scheitert (der Spion antwortet 500) — hier zählt allein, was gesendet wurde.
    let _ = connect_with_policy(&cfg(&url, headers), &policy).await;

    let got = String::from_utf8_lossy(&seen.lock().await.clone()).to_lowercase();
    assert!(
        got.contains("authorization: bearer sk-auf-der-leitung"),
        "Header fehlt im Request:\n{got}"
    );
}

#[tokio::test]
async fn without_a_net_grant_nothing_is_sent_at_all() {
    std::env::set_var("SEPP_TEST_WIRE_TOKEN2", "sk-darf-nicht-raus");
    let (url, seen) = spy().await;
    let mut headers = BTreeMap::new();
    headers.insert(
        "authorization".to_string(),
        "Bearer $SEPP_TEST_WIRE_TOKEN2".to_string(),
    );
    // env gewährt, net nicht → der Connect muss abbrechen, bevor eine Verbindung entsteht.
    let policy = Policy::new(vec![Capability::Env {
        name: "SEPP_TEST_WIRE_TOKEN2".into(),
    }]);

    let msg = match connect_with_policy(&cfg(&url, headers), &policy).await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("ohne net-Gewährung darf gar nicht verbunden werden"),
    };
    assert!(msg.contains("nicht gewährt"), "{msg}");
    assert!(!msg.contains("sk-darf-nicht-raus"), "{msg}");
    assert!(
        seen.lock().await.is_empty(),
        "es wurde eine Verbindung aufgebaut, obwohl der Host nicht gewährt ist"
    );
}
