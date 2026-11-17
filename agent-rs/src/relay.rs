// Keeps an outbound connection to the relay, so the PC is reachable from anywhere without opening
// ports. For each viewer the relay asks us to dial back a channel socket, which is attached to the
// hub exactly like a browser socket.
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::config::{Config, Shared, Status};
use crate::hub::Hub;
use crate::util::uri_encode;

/// The relay pings every 5 s; this much silence means the connection is dead.
const SILENCE: Duration = Duration::from_secs(12);

fn request(cfg: &Config, path: &str) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    let mut req = format!("{}{path}", cfg.ws_base()).into_client_request()?;
    let h = req.headers_mut();
    h.insert("Authorization", HeaderValue::from_str(&format!("Bearer {}", cfg.api_key.trim()))?);
    h.insert("X-Agent-Name", HeaderValue::from_str(&uri_encode(cfg.name.trim()))?);
    h.insert("X-Machine-Id", HeaderValue::from_str(&cfg.machine_id)?);
    h.insert("User-Agent", HeaderValue::from_static(concat!("ldremote/", env!("CARGO_PKG_VERSION"))));
    Ok(req)
}

pub(crate) async fn connect(cfg: &Config, path: &str) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, WsError> {
    let req = request(cfg, path).map_err(|e| WsError::Io(std::io::Error::other(e.to_string())))?;
    let (ws, _) = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| WsError::Io(std::io::Error::other("timed out")))??;
    if let tokio_tungstenite::MaybeTlsStream::Plain(s) = ws.get_ref() {
        let _ = s.set_nodelay(true);
    }
    if let tokio_tungstenite::MaybeTlsStream::Rustls(s) = ws.get_ref() {
        let _ = s.get_ref().0.set_nodelay(true);
    }
    Ok(ws)
}

pub async fn run(shared: Arc<Shared>, hub: Arc<Hub>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let cfg = shared.config();
        if !cfg.is_complete() {
            shared.set_status(Status::NeedsKey);
            shared.reconnect.notified().await;
            continue;
        }
        shared.set_status(Status::Connecting);
        let result = tokio::select! {
            r = session(&cfg, &shared, &hub) => r,
            _ = shared.reconnect.notified() => {
                tracing::info!("settings changed, reconnecting");
                backoff = Duration::from_secs(1);
                continue;
            }
        };
        let wait = match result {
            Ok(()) => {
                backoff = Duration::from_secs(1);
                shared.set_status(Status::Offline("server đóng kết nối".into()));
                backoff
            }
            Err(Failure::Unauthorized) => {
                tracing::warn!("relay refused the API key");
                shared.set_status(Status::BadKey);
                Duration::from_secs(60)
            }
            Err(Failure::Other(e)) => {
                tracing::warn!("relay: {e}");
                shared.set_status(Status::Offline(e));
                backoff
            }
        };
        backoff = (backoff * 2).min(Duration::from_secs(30));
        // A settings change cuts the wait short.
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shared.reconnect.notified() => backoff = Duration::from_secs(1),
        }
    }
}

enum Failure {
    Unauthorized,
    Other(String),
}

async fn session(cfg: &Config, shared: &Arc<Shared>, hub: &Arc<Hub>) -> Result<(), Failure> {
    let ws = match connect(cfg, "/agent").await {
        Ok(ws) => ws,
        Err(WsError::Http(resp)) if resp.status() == 401 => return Err(Failure::Unauthorized),
        Err(e) => return Err(Failure::Other(e.to_string())),
    };
    tracing::info!("connected to relay {} as \"{}\"", cfg.ws_base(), cfg.name);
    shared.set_status(Status::Connected);
    let (mut sink, mut source) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });

    let result = loop {
        let msg = match tokio::time::timeout(SILENCE, source.next()).await {
            Err(_) => break Err(Failure::Other("relay im lặng quá lâu".into())),
            Ok(None) => break Ok(()),
            Ok(Some(Err(e))) => break Err(Failure::Other(e.to_string())),
            Ok(Some(Ok(m))) => m,
        };
        let Message::Text(text) = msg else { continue };
        let Ok(msg) = serde_json::from_str::<Value>(&text) else { continue };
        match msg.get("type").and_then(Value::as_str) {
            Some("devices") => {
                let hub = hub.clone();
                let out = out_tx.clone();
                let req_id = msg.get("reqId").cloned().unwrap_or(Value::Null);
                tokio::spawn(async move {
                    let devices = hub.ld.list_devices().await;
                    let _ = out.send(Message::Text(json!({ "type": "devices", "reqId": req_id, "devices": devices }).to_string().into()));
                });
            }
            Some("open") => {
                let (Some(channel), Some(serial)) = (msg.get("channel").and_then(Value::as_str), msg.get("serial").and_then(Value::as_str)) else {
                    continue;
                };
                let cfg = cfg.clone();
                let hub = hub.clone();
                let (channel, serial) = (channel.to_string(), serial.to_string());
                // ICE servers for this viewer (TURN credentials are minted per viewer by the relay).
                let ice = msg.get("ice").and_then(|i| i.get("browser")).filter(|b| b.is_array()).cloned();
                tokio::spawn(async move {
                    match connect(&cfg, &format!("/agent/channel?id={}", uri_encode(&channel))).await {
                        Ok(ws) => hub.attach(serial, ws, ice),
                        Err(e) => tracing::warn!("channel {}: {e}", &channel[..channel.len().min(8)]),
                    }
                });
            }
            Some("cmd") => crate::manage::handle(msg, cfg.clone(), hub.clone(), out_tx.clone()),
            _ => {}
        }
    };
    writer.abort();
    result
}
