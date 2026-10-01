//! Telegram alerts + operator commands (only from the configured chat).

use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Status,
    Positions,
    Pause,
    Resume,
    Kill,
    Flatten,
    Leaders,
    Help,
}

impl Command {
    pub fn parse(text: &str) -> Option<Self> {
        let cmd = text
            .split_whitespace()
            .next()?
            .split('@')
            .next()?
            .to_lowercase();
        Some(match cmd.as_str() {
            "/status" => Command::Status,
            "/positions" | "/pos" => Command::Positions,
            "/pause" => Command::Pause,
            "/resume" => Command::Resume,
            "/kill" => Command::Kill,
            "/flatten" => Command::Flatten,
            "/leaders" => Command::Leaders,
            "/help" | "/start" => Command::Help,
            _ => return None,
        })
    }
}

pub const HELP: &str = "/status – engine health, PnL, latency\n/positions – open positions\n/leaders – copied wallets\n/pause – stop new entries (exits keep running)\n/resume – allow entries\n/kill – kill switch: no entries until restart\n/flatten – sell everything now";

#[derive(Clone)]
pub struct Telegram {
    token: String,
    chat_id: i64,
    http: reqwest::Client,
}

impl Telegram {
    pub fn new(token: String, chat_id: i64) -> Self {
        Self {
            token,
            chat_id,
            http: chain::rpc::http_client(Duration::from_secs(35)),
        }
    }

    pub fn send(&self, text: impl Into<String>) {
        let me = self.clone();
        let text = text.into();
        tokio::spawn(async move {
            let url = format!("https://api.telegram.org/bot{}/sendMessage", me.token);
            let body =
                json!({"chat_id": me.chat_id, "text": text, "disable_web_page_preview": true});
            if let Err(e) = me.http.post(url).json(&body).send().await {
                tracing::warn!("telegram send: {e}");
            }
        });
    }

    /// Long-poll for commands and forward them to the engine.
    pub fn spawn_commands(&self, out: mpsc::Sender<Command>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut offset: i64 = 0;
            loop {
                let url = format!("https://api.telegram.org/bot{}/getUpdates", me.token);
                let res = me
                    .http
                    .get(&url)
                    .query(&[("timeout", "30"), ("offset", &offset.to_string())])
                    .send()
                    .await;
                let v: Value = match res {
                    Ok(r) => r.json().await.unwrap_or_default(),
                    Err(e) => {
                        tracing::warn!("telegram poll: {e}");
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        continue;
                    }
                };
                for u in v["result"].as_array().into_iter().flatten() {
                    offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
                    let msg = &u["message"];
                    if msg["chat"]["id"].as_i64() != Some(me.chat_id) {
                        continue; // ignore everyone else
                    }
                    if let Some(c) = msg["text"].as_str().and_then(Command::parse) {
                        let _ = out.send(c).await;
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(Command::parse("/status"), Some(Command::Status));
        assert_eq!(Command::parse("/Flatten@mybot now"), Some(Command::Flatten));
        assert_eq!(Command::parse("hello"), None);
    }
}
