use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite;

const RECONNECT_MIN_SECS: u64 = 3;
const RECONNECT_MAX_SECS: u64 = 30;

#[derive(Debug)]
pub struct MempoolPush {
    pub txid: String,
    pub raw_hex: String,
}

/// Converts an HTTP(S) API URL to its WebSocket equivalent.
pub fn api_url_to_ws(api_url: &str) -> String {
    if api_url.starts_with("https://") {
        api_url.replacen("https://", "wss://", 1)
    } else if api_url.starts_with("http://") {
        api_url.replacen("http://", "ws://", 1)
    } else {
        format!("ws://{}", api_url)
    }
}

/// Long-running task: connect to CipherScan WebSocket, subscribe to raw_mempool,
/// and forward mempool transactions with raw_hex through the channel.
/// Reconnects automatically on disconnect with exponential backoff.
pub async fn run(ws_url: String, service_key: String, tx: mpsc::Sender<MempoolPush>) {
    let mut delay = std::time::Duration::from_secs(RECONNECT_MIN_SECS);

    loop {
        tracing::info!(url = %ws_url, "[WS] Connecting to CipherScan...");

        match connect(&ws_url, &service_key).await {
            Ok(mut stream) => {
                delay = std::time::Duration::from_secs(RECONNECT_MIN_SECS);

                use futures::SinkExt;
                let sub = serde_json::json!({"subscribe": "raw_mempool"});
                if let Err(e) = stream.send(tungstenite::Message::Text(sub.to_string().into())).await {
                    tracing::warn!(error = %e, "[WS] Failed to send subscribe");
                    continue;
                }

                tracing::info!("[WS] Subscribed to raw_mempool — waiting for transactions");

                while let Some(msg) = stream.next().await {
                    match msg {
                        Ok(tungstenite::Message::Text(text)) => {
                            if let Some(push) = parse_mempool_tx(&text) {
                                if tx.send(push).await.is_err() {
                                    tracing::info!("[WS] Channel closed, stopping");
                                    return;
                                }
                            }
                        }
                        Ok(tungstenite::Message::Close(_)) => {
                            tracing::info!("[WS] Server closed connection");
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "[WS] Stream error");
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "[WS] Connect failed");
            }
        }

        tracing::info!(delay_secs = delay.as_secs(), "[WS] Reconnecting...");
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(std::time::Duration::from_secs(RECONNECT_MAX_SECS));
    }
}

async fn connect(
    url: &str,
    service_key: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    String,
> {
    use tungstenite::client::IntoClientRequest;

    let mut request = url
        .into_client_request()
        .map_err(|e| format!("Invalid WS URL: {}", e))?;

    request.headers_mut().insert(
        "X-Service-Key",
        service_key.parse().map_err(|e| format!("Invalid service key header: {}", e))?,
    );

    let (stream, _resp) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| format!("WS handshake failed: {}", e))?;

    Ok(stream)
}

fn parse_mempool_tx(text: &str) -> Option<MempoolPush> {
    let msg: serde_json::Value = serde_json::from_str(text).ok()?;

    if msg.get("type")?.as_str()? != "mempool_tx" {
        return None;
    }

    let data = msg.get("data")?;
    let txid = data.get("txid")?.as_str()?.to_string();
    let raw_hex = data.get("raw_hex")?.as_str()?;

    if raw_hex.is_empty() {
        return None;
    }

    Some(MempoolPush {
        txid,
        raw_hex: raw_hex.to_string(),
    })
}
