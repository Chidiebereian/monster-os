//! WebSocket pass-through: GET /ws/{service}/{path..} upgrades the client
//! connection, opens one to the upstream service, and relays frames both ways
//! until either side closes. Only services listed in WS_SERVICES are reachable.
//! The client's Origin and address (X-Forwarded-For) go upstream so the
//! service can apply its own origin and local-only checks.

use actix_web::{web, HttpRequest, HttpResponse};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
use tokio_tungstenite::tungstenite::Message as Up;

use crate::Gateway;

/// Services that speak WebSocket.
pub const WS_SERVICES: &[&str] = &["monster-gpt"];

/// http://host:port -> ws://host:port (https -> wss).
pub fn ws_base(http_base: &str) -> String {
    if let Some(r) = http_base.strip_prefix("https://") { format!("wss://{r}") }
    else if let Some(r) = http_base.strip_prefix("http://") { format!("ws://{r}") }
    else { http_base.to_string() }
}

pub async fn ws_proxy(gw: web::Data<Gateway>, req: HttpRequest, body: web::Payload, path: web::Path<(String, String)>) -> Result<HttpResponse, actix_web::Error> {
    let (service, tail) = path.into_inner();
    if !WS_SERVICES.contains(&service.as_str()) {
        return Ok(HttpResponse::NotFound().json(serde_json::json!({ "error": format!("{service:?} has no WebSocket endpoint") })));
    }
    let Some(base) = gw.upstreams.get(service.as_str()) else { return Ok(HttpResponse::NotFound().finish()) };
    let mut url = format!("{}/{tail}", ws_base(base));
    if !req.query_string().is_empty() { url.push('?'); url.push_str(req.query_string()); }

    let mut up_req = match url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => return Ok(HttpResponse::BadGateway().json(serde_json::json!({ "error": format!("bad upstream url: {e}") }))),
    };
    if let Some(o) = req.headers().get("origin").and_then(|o| o.to_str().ok()).and_then(|o| o.parse().ok()) { up_req.headers_mut().insert("origin", o); }
    if let Some(ip) = req.peer_addr().map(|a| a.ip().to_string()) {
        if let Ok(v) = ip.parse() { up_req.headers_mut().insert("x-forwarded-for", v); }
    }
    // connect upstream first, so a refused or down service is a clean HTTP error
    let (upstream, _) = match tokio_tungstenite::connect_async(up_req).await {
        Ok(x) => x,
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => {
            let code = actix_web::http::StatusCode::from_u16(r.status().as_u16()).unwrap_or(actix_web::http::StatusCode::BAD_GATEWAY);
            return Ok(HttpResponse::build(code).json(serde_json::json!({ "error": format!("{service} refused the WebSocket ({})", r.status()) })));
        }
        Err(e) => {
            eprintln!("api-gateway: ws {service} unreachable at {url}: {e}");
            return Ok(HttpResponse::BadGateway().json(serde_json::json!({ "error": format!("{service} unreachable") })));
        }
    };
    let (resp, mut session, mut client) = actix_ws::handle(&req, body)?;
    actix_web::rt::spawn(async move {
        let (mut up_tx, mut up_rx) = upstream.split();
        loop {
            tokio::select! {
                m = client.recv() => {
                    let fwd = match m {
                        Some(Ok(actix_ws::Message::Text(t))) => Up::Text(t.to_string().into()),
                        Some(Ok(actix_ws::Message::Binary(b))) => Up::Binary(b.to_vec().into()),
                        Some(Ok(actix_ws::Message::Ping(b))) => Up::Ping(b.to_vec().into()),
                        Some(Ok(actix_ws::Message::Pong(b))) => Up::Pong(b.to_vec().into()),
                        Some(Ok(actix_ws::Message::Continuation(_) | actix_ws::Message::Nop)) => continue,
                        Some(Ok(actix_ws::Message::Close(_))) | Some(Err(_)) | None => {
                            let _ = up_tx.send(Up::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "client closed".into() }))).await;
                            break;
                        }
                    };
                    if up_tx.send(fwd).await.is_err() { let _ = session.close(None).await; break; }
                }
                m = up_rx.next() => {
                    let ok = match m {
                        Some(Ok(Up::Text(t))) => session.text(t.to_string()).await.is_ok(),
                        Some(Ok(Up::Binary(b))) => session.binary(b.to_vec()).await.is_ok(),
                        Some(Ok(Up::Ping(b))) => session.ping(&b).await.is_ok(),
                        Some(Ok(Up::Pong(b))) => session.pong(&b).await.is_ok(),
                        Some(Ok(Up::Frame(_))) => true,
                        Some(Ok(Up::Close(_))) | Some(Err(_)) | None => { let _ = session.close(None).await; break; }
                    };
                    if !ok { let _ = up_tx.send(Up::Close(None)).await; break; }
                }
            }
        }
    });
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_base_swaps_scheme() {
        assert_eq!(ws_base("http://localhost:8085"), "ws://localhost:8085");
        assert_eq!(ws_base("https://gpt.internal"), "wss://gpt.internal");
    }
}
