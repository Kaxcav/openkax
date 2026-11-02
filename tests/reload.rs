//! F0-10: hot-reload do `providers.toml` e snapshot JSON dos buckets.

mod common;

use std::time::{Duration, Instant};

use common::*;
use openkax::config::Config;
use openkax::provider::Secrets;
use openkax::server;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer};

const KEYS: &[(&str, &str)] = &[("K", "k")];

/// Dois providers `a` e `b` (modelo `m`, 100 RPD no modelo) e a classe
/// `kax/coder-fast` na ordem pedida.
fn config(a: &str, b: &str, order: [&str; 2]) -> String {
    let mut t =
        String::from("[defaults]\nfirst_token_timeout_ms = 2000\nrequest_timeout_ms = 5000\n");
    for (id, uri) in [("a", a), ("b", b)] {
        t.push_str(&format!(
            "[[provider]]\nid = \"{id}\"\nbase_url = \"{uri}\"\nauth = {{ kind = \"bearer\", env = \"K\" }}\nmodels = [{{ id = \"m\", upstream = \"{id}-up\", limits = {{ rpd = 100 }} }}]\n"
        ));
    }
    t.push_str(&format!(
        "[[class]]\nid = \"kax/coder-fast\"\nmax_packet_tokens = 60000\nroute = [{{ provider = \"{}\", model = \"m\" }}, {{ provider = \"{}\", model = \"m\" }}]\n",
        order[0], order[1]
    ));
    t
}

async fn ok_mock(server: &MockServer, upstream: &str, text: &str) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({ "model": upstream })))
        .respond_with(sse_ok(text))
        .mount(server)
        .await;
}

fn rpd(q: &Value, provider: usize) -> Option<i64> {
    q["providers"][provider]["models"][0]["rpd_remaining"].as_i64()
}

/// `reload` troca a rota na hora e leva os buckets da geração anterior.
#[tokio::test]
async fn reload_swaps_route_and_keeps_buckets() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    ok_mock(&a, "a-up", "from-a").await;
    ok_mock(&b, "b-up", "from-b").await;
    let gw = Gateway::spawn(&config(&a.uri(), &b.uri(), ["a", "b"]), KEYS).await;

    let r = gw.chat_stream("kax/coder-fast", "oi").await;
    assert_eq!(r.provider.as_deref(), Some("a"), "{r:?}");
    assert_eq!(rpd(&gw.quota().await, 0), Some(99));

    let next = Config::from_toml_str(&config(&a.uri(), &b.uri(), ["b", "a"])).unwrap();
    gw.state
        .reload(next, Secrets::from_pairs(KEYS.iter().copied()))
        .unwrap();

    let r = gw.chat_stream("kax/coder-fast", "oi").await;
    assert_eq!(r.provider.as_deref(), Some("b"), "{r:?}");
    let q = gw.quota().await;
    assert_eq!(
        rpd(&q, 0),
        Some(99),
        "bucket de a sobreviveu ao reload: {q}"
    );
    assert_eq!(rpd(&q, 1), Some(99));
}

/// Snapshot em arquivo: salvo por um processo, restaurado no start do próximo.
#[tokio::test]
async fn state_file_round_trip_across_restart() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    ok_mock(&a, "a-up", "from-a").await;
    let toml = config(&a.uri(), &b.uri(), ["a", "b"]);
    let dir = tempdir("state");
    let file = dir.join("state").join("buckets.json");

    let gw = Gateway::spawn(&toml, KEYS).await;
    for _ in 0..3 {
        gw.chat_stream("kax/coder-fast", "oi").await;
    }
    gw.state.save_state(&file).unwrap();
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["scopes"]["a/m"]["rpd"]["used"], 3, "{saved}");

    let restarted = Gateway::spawn(&toml, KEYS).await;
    assert_eq!(rpd(&restarted.quota().await, 0), Some(100));
    assert!(restarted.state.restore_state(&file).unwrap());
    assert_eq!(rpd(&restarted.quota().await, 0), Some(97));

    // Sem arquivo: nada a restaurar, sem erro. Corrompido: erro, estado intacto.
    assert!(
        !restarted
            .state
            .restore_state(&dir.join("nao-existe.json"))
            .unwrap()
    );
    let bad = dir.join("ruim.json");
    std::fs::write(&bad, "{ não é json").unwrap();
    assert!(restarted.state.restore_state(&bad).is_err());
    assert_eq!(rpd(&restarted.quota().await, 0), Some(97));
}

/// O watcher recarrega quando o arquivo muda; TOML inválido mantém a config viva.
#[tokio::test]
async fn watcher_reloads_on_file_change_and_ignores_invalid_toml() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    let dir = tempdir("watch");
    let file = dir.join("providers.toml");
    let toml = config(&a.uri(), &b.uri(), ["a", "b"]);
    std::fs::write(&file, &toml).unwrap();
    let gw = Gateway::spawn(&toml, KEYS).await;
    let _watcher = server::watch_config(file.clone(), gw.state.clone(), |_: &Config| {
        Secrets::from_pairs(KEYS.iter().copied())
    })
    .unwrap();

    let extra = format!(
        "{toml}[[class]]\nid = \"kax/extra\"\nmax_packet_tokens = 100\nroute = [{{ provider = \"a\", model = \"m\" }}]\n"
    );
    std::fs::write(&file, &extra).unwrap();
    wait_until(|| async { gw.class_ids().await.contains(&"kax/extra".to_owned()) }).await;

    std::fs::write(&file, "isto não é toml [[[").unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(gw.class_ids().await.contains(&"kax/extra".to_owned()));

    std::fs::write(&file, &toml).unwrap();
    wait_until(|| async { !gw.class_ids().await.contains(&"kax/extra".to_owned()) }).await;
}

async fn wait_until<F, Fut>(mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(10) {
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("condição não ficou verdadeira em 10 s");
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "openkax-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}
