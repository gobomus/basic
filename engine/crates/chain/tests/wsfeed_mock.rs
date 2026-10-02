//! The free feed against a mock WebSocket + HTTP RPC: subscriptions, log
//! pre-filtering, transaction fetching, event order and reconnection.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chain::detect;
use chain::feed::{FeedEvent, Filters};
use chain::model::ChainTx;
use chain::rpc::Rpc;
use chain::wsfeed::{self, WsConfig};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use tungstenite::Message;

fn fixture() -> Value {
    let p = format!(
        "{}/tests/fixtures/pump_curve_1.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    v.get("result").cloned().unwrap_or(v)
}

/// Minimal JSON-RPC-over-HTTP server: getTransaction serves the fixture, anything else is empty.
fn http_server(tx: Value, fetches: Arc<AtomicUsize>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let (tx, fetches) = (tx.clone(), fetches.clone());
            std::thread::spawn(move || serve_http(s, tx, fetches));
        }
    });
    url
}

fn serve_http(mut s: TcpStream, tx: Value, fetches: Arc<AtomicUsize>) {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    loop {
        let mut buf = Vec::new();
        let mut b = [0u8; 4096];
        let (head_end, len) = loop {
            let Ok(n) = s.read(&mut b) else { return };
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&b[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                break (i + 4, len);
            }
        };
        while buf.len() < head_end + len {
            let Ok(n) = s.read(&mut b) else { return };
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&b[..n]);
        }
        let req: Value =
            serde_json::from_slice(&buf[head_end..head_end + len]).unwrap_or(Value::Null);
        let result = match req["method"].as_str() {
            Some("getTransaction") => {
                fetches.fetch_add(1, Ordering::SeqCst);
                tx.clone()
            }
            Some("getMultipleAccounts") => json!({"context": {"slot": 5}, "value": [null]}),
            _ => Value::Null,
        };
        let body = json!({"jsonrpc": "2.0", "id": req["id"], "result": result}).to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        if s.write_all(resp.as_bytes()).is_err() {
            return;
        }
    }
}

fn logs_notification(sub: u64, sig: &str, program: &str, err: Value) -> String {
    json!({"jsonrpc": "2.0", "method": "logsNotification", "params": {"result": {
        "context": {"slot": 100},
        "value": {"signature": sig, "err": err, "logs": [format!("Program {program} invoke [1]")]}},
        "subscription": sub}})
    .to_string()
}

/// WebSocket server: acks subscriptions, then for the first logsSubscribe sends a slot update,
/// a failed tx, a non-swap tx and a good swap tx; `drop_after_first` closes the connection after that.
fn ws_server(sessions: Arc<AtomicUsize>, drop_after_first: bool) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let n = sessions.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                let Ok(mut ws) = tungstenite::accept(s) else {
                    return;
                };
                let mut next_sub = 100;
                loop {
                    let t = match ws.read() {
                        Ok(Message::Text(t)) => t,
                        Ok(Message::Close(_)) | Err(_) => return,
                        Ok(_) => continue, // ping / pong / binary
                    };
                    let v: Value = serde_json::from_str(&t).unwrap();
                    let id = v["id"].clone();
                    match v["method"].as_str() {
                        Some("slotSubscribe") => {
                            ws.send(Message::text(
                                json!({"jsonrpc":"2.0","result":7,"id":id}).to_string(),
                            ))
                            .unwrap();
                            ws.send(Message::text(json!({"jsonrpc":"2.0","method":"slotNotification","params":{"result":{"slot":123,"parent":122,"root":100},"subscription":7}}).to_string())).unwrap();
                        }
                        Some("logsSubscribe") => {
                            next_sub += 1;
                            ws.send(Message::text(
                                json!({"jsonrpc":"2.0","result":next_sub,"id":id}).to_string(),
                            ))
                            .unwrap();
                            let pump = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
                            // failed transaction: ignored
                            ws.send(Message::text(logs_notification(
                                next_sub,
                                "FAILED",
                                pump,
                                json!({"InstructionError":[0,"Custom"]}),
                            )))
                            .unwrap();
                            // not a swap program: ignored without any RPC call
                            ws.send(Message::text(logs_notification(
                                next_sub,
                                "TRANSFER",
                                "11111111111111111111111111111111",
                                Value::Null,
                            )))
                            .unwrap();
                            // a real swap: fetched, decoded, delivered (twice: deduplicated)
                            let sig = format!("SWAP{n}");
                            ws.send(Message::text(logs_notification(
                                next_sub,
                                &sig,
                                pump,
                                Value::Null,
                            )))
                            .unwrap();
                            ws.send(Message::text(logs_notification(
                                next_sub,
                                &sig,
                                pump,
                                Value::Null,
                            )))
                            .unwrap();
                            if drop_after_first && n == 0 {
                                std::thread::sleep(Duration::from_millis(300));
                                return; // abrupt close: the feed must reconnect and resubscribe
                            }
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    url
}

async fn collect(rx: &mut mpsc::Receiver<FeedEvent>, secs: u64) -> Vec<FeedEvent> {
    let mut out = vec![];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        out.push(ev);
    }
    out
}

fn start(
    ws_url: &str,
    rpc_url: &str,
    leader: &str,
) -> (mpsc::Receiver<FeedEvent>, Vec<tokio::task::JoinHandle<()>>) {
    std::env::set_var("WSFEED_MOCK_WS", ws_url);
    let cfg = WsConfig {
        url_env: Some("WSFEED_MOCK_WS".into()),
        ..Default::default()
    };
    let (_ftx, frx) = watch::channel(Filters {
        leaders: vec![leader.to_string()],
        mints: vec![],
    });
    // keep the sender alive for the test's duration
    Box::leak(Box::new(_ftx));
    let (_wtx, wrx) = watch::channel(Vec::new());
    Box::leak(Box::new(_wtx));
    let (tx, rx) = mpsc::channel(1000);
    (rx, wsfeed::spawn(cfg, Rpc::new(rpc_url), frx, wrx, tx))
}

#[tokio::test]
async fn delivers_swaps_slots_and_ignores_noise() {
    let fx = fixture();
    let tx = ChainTx::from_rpc_json(&fx).unwrap();
    let leader = detect::all_swaps(&tx)[0].wallet.to_string();
    let fetches = Arc::new(AtomicUsize::new(0));
    let rpc = http_server(fx, fetches.clone());
    let sessions = Arc::new(AtomicUsize::new(0));
    let ws = ws_server(sessions.clone(), false);
    let (mut rx, handles) = start(&ws, &rpc, &leader);

    let events = collect(&mut rx, 3).await;
    handles.iter().for_each(|h| h.abort());

    assert!(
        events.iter().any(
            |e| matches!(e, FeedEvent::Status { connected: true, detail, .. } if detail.is_empty())
        ),
        "connected"
    );
    assert!(events.iter().any(|e| matches!(e, FeedEvent::Status { detail, .. } if detail == &format!("subscribed {leader}"))), "leader subscription acknowledged");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, FeedEvent::Slot { slot: 123, .. })),
        "slot updates flow"
    );
    let txs: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let FeedEvent::Tx(t) = e {
                Some(t)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        txs.len(),
        1,
        "one swap delivered: failed and non-swap logs skipped, duplicate deduplicated"
    );
    assert_eq!(txs[0].signature, tx.signature);
    assert!(
        txs[0].observed_at_ns > 0,
        "observation time is the notification time"
    );
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        1,
        "exactly one getTransaction call for the whole burst"
    );
}

#[tokio::test]
async fn reconnects_and_resubscribes_after_a_dropped_connection() {
    let fx = fixture();
    let tx = ChainTx::from_rpc_json(&fx).unwrap();
    let leader = detect::all_swaps(&tx)[0].wallet.to_string();
    let rpc = http_server(fx, Arc::new(AtomicUsize::new(0)));
    let sessions = Arc::new(AtomicUsize::new(0));
    let ws = ws_server(sessions.clone(), true);
    let (mut rx, handles) = start(&ws, &rpc, &leader);

    let events = collect(&mut rx, 5).await;
    handles.iter().for_each(|h| h.abort());

    assert!(sessions.load(Ordering::SeqCst) >= 2, "the feed reconnected");
    assert!(
        events.iter().any(|e| matches!(
            e,
            FeedEvent::Status {
                connected: false,
                ..
            }
        )),
        "the drop is reported"
    );
    let subscribed = events.iter().filter(|e| matches!(e, FeedEvent::Status { detail, .. } if detail == &format!("subscribed {leader}"))).count();
    assert!(
        subscribed >= 2,
        "the leader was resubscribed on the new connection ({subscribed})"
    );
    // each connection's SWAP signature is distinct, so both are delivered
    let n = events
        .iter()
        .filter(|e| matches!(e, FeedEvent::Tx(_)))
        .count();
    assert!(n >= 2, "swaps keep flowing after the reconnect ({n})");
}
