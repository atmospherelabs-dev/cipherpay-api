use serde::de::DeserializeOwned;
use serde::Deserialize;

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
    meta: serde_json::Value,
}

/// Explorer configuration is a host URL; tolerate an explicit /v1 base too.
pub(super) fn base_url(api_url: &str) -> &str {
    api_url.trim_end_matches('/').trim_end_matches("/v1")
}

pub(super) async fn get<T: DeserializeOwned>(
    http: &reqwest::Client,
    api_url: &str,
    path: &str,
) -> anyhow::Result<T> {
    let url = format!("{}/v1/{}", base_url(api_url), path);
    let envelope: Envelope<T> = http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    anyhow::ensure!(envelope.meta.is_object(), "Invalid ZecBlock v1 metadata");
    Ok(envelope.data)
}

#[cfg(test)]
mod tests {
    use crate::scanner::{blocks, mempool};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn fixture(responses: Vec<(&'static str, u16, serde_json::Value)>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            for (path, status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 1024];
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let correct_path = String::from_utf8_lossy(&request)
                    .starts_with(&format!("GET {path} HTTP/1.1\r\n"));
                let status = if correct_path { status } else { 400 };
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        base
    }

    fn envelope(data: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"data": data, "meta": {"network": "testnet"}})
    }

    #[tokio::test]
    async fn scanner_reads_v1_paths_and_envelopes() {
        let base = fixture(vec![
            (
                "/v1/mempool",
                200,
                envelope(serde_json::json!({"transactions": [{"txid": "abc"}]})),
            ),
            (
                "/v1/transactions/abc/raw",
                200,
                envelope(serde_json::json!({"hex": "deadbeef"})),
            ),
            (
                "/v1/network/blockchain-info",
                200,
                envelope(serde_json::json!({"blocks": 100})),
            ),
            (
                "/v1/blocks/100",
                200,
                envelope(serde_json::json!({"transactions": [{"txid": "abc"}]})),
            ),
            (
                "/v1/transactions/abc",
                200,
                envelope(serde_json::json!({"blockHeight": "100", "confirmations": 1})),
            ),
            (
                "/v1/transactions/abc",
                200,
                envelope(serde_json::json!({"blockHeight": "100", "confirmations": 0})),
            ),
            (
                "/v1/transactions/abc/raw",
                200,
                envelope(serde_json::json!({"hex": "deadbeef"})),
            ),
        ])
        .await;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        assert_eq!(
            mempool::fetch_mempool_txids(&http, &base).await.unwrap(),
            vec!["abc"]
        );
        assert_eq!(
            mempool::fetch_raw_tx(&http, &format!("{base}/v1/"), "abc")
                .await
                .unwrap(),
            "deadbeef"
        );
        assert_eq!(blocks::get_chain_height(&http, &base).await.unwrap(), 100);
        assert_eq!(
            blocks::fetch_block_txids(&http, &base, 100, 100)
                .await
                .unwrap(),
            vec!["abc"]
        );
        assert!(blocks::check_tx_confirmed(&http, &base, "abc")
            .await
            .unwrap());
        assert!(!blocks::check_tx_confirmed(&http, &base, "abc")
            .await
            .unwrap());
        assert_eq!(
            mempool::fetch_raw_txs_batch(&http, &base, &["abc".into()]).await,
            vec![("abc".into(), "deadbeef".into())]
        );
    }

    #[tokio::test]
    async fn errors_and_malformed_payloads_cannot_look_like_empty_mempools() {
        let base = fixture(vec![
            (
                "/v1/mempool",
                401,
                envelope(serde_json::json!({"transactions": []})),
            ),
            (
                "/v1/mempool",
                429,
                serde_json::json!({"title": "Too many requests"}),
            ),
            ("/v1/mempool", 200, serde_json::json!({"transactions": []})),
            ("/v1/mempool", 200, envelope(serde_json::json!({}))),
            (
                "/v1/mempool",
                200,
                serde_json::json!({"data": {"transactions": []}, "meta": null}),
            ),
            (
                "/v1/mempool",
                200,
                envelope(serde_json::json!({"transactions": []})),
            ),
        ])
        .await;
        let http = reqwest::Client::new();
        for _ in 0..5 {
            assert!(mempool::fetch_mempool_txids(&http, &base).await.is_err());
        }
        assert!(mempool::fetch_mempool_txids(&http, &base)
            .await
            .unwrap()
            .is_empty());
    }
}
