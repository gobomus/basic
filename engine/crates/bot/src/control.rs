//! Local operator control over a Unix socket (no external services).
//! `copybot ctl <command>` connects, sends one line, prints the engine's reply.

use std::path::Path;

use chain::solana_sdk::pubkey::Pubkey;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Status,
    Positions,
    Pause,
    Resume,
    Kill,
    Flatten,
    Leaders,
    /// List the blacklist, or add / remove a token mint or dev wallet.
    Blacklist(Option<Pubkey>),
    Unblacklist(Pubkey),
}

impl Command {
    pub fn parse(text: &str) -> Option<Self> {
        let mut words = text.split_whitespace();
        let verb = words.next()?.to_lowercase();
        // addresses are case-sensitive base58: only the verb is lowercased
        let arg = words.next().map(|a| a.parse::<Pubkey>());
        if words.next().is_some() {
            return None;
        }
        match (verb.as_str(), arg) {
            ("blacklist", None) => return Some(Command::Blacklist(None)),
            ("blacklist", Some(Ok(pk))) => return Some(Command::Blacklist(Some(pk))),
            ("unblacklist", Some(Ok(pk))) => return Some(Command::Unblacklist(pk)),
            (_, Some(_)) => return None,
            _ => {}
        }
        Some(match verb.as_str() {
            "status" => Command::Status,
            "positions" | "pos" => Command::Positions,
            "pause" => Command::Pause,
            "resume" => Command::Resume,
            "kill" => Command::Kill,
            "flatten" => Command::Flatten,
            "leaders" => Command::Leaders,
            _ => return None,
        })
    }
}

pub const HELP: &str = "commands: status | positions | leaders | pause | resume | kill | flatten | blacklist [<mint|dev>] | unblacklist <mint|dev>";

pub type Request = (Command, oneshot::Sender<String>);

/// Serve the control socket; each connection carries one command.
pub fn serve(path: &str, out: mpsc::Sender<Request>) -> anyhow::Result<()> {
    if let Some(dir) = Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(path); // stale socket from a previous run
    let listener = UnixListener::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let out = out.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut line = String::new();
                if BufReader::new(r).read_line(&mut line).await.is_err() {
                    return;
                }
                let reply = match Command::parse(&line) {
                    Some(c) => {
                        let (tx, rx) = oneshot::channel();
                        if out.send((c, tx)).await.is_err() {
                            "engine not running".to_string()
                        } else {
                            rx.await.unwrap_or_else(|_| "no reply".into())
                        }
                    }
                    None => HELP.to_string(),
                };
                let _ = w.write_all(reply.as_bytes()).await;
                let _ = w.write_all(b"\n").await;
            });
        }
    });
    Ok(())
}

/// Client side used by `copybot ctl`.
pub async fn send(path: &str, cmd: &str) -> anyhow::Result<String> {
    let mut s = UnixStream::connect(path)
        .await
        .map_err(|e| anyhow::anyhow!("cannot reach engine at {path}: {e}"))?;
    s.write_all(format!("{cmd}\n").as_bytes()).await?;
    let mut out = String::new();
    s.read_to_string(&mut out).await?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        assert_eq!(Command::parse("status\n"), Some(Command::Status));
        assert_eq!(Command::parse(" FLATTEN "), Some(Command::Flatten));
        assert_eq!(Command::parse("hello"), None);
    }

    #[tokio::test]
    async fn round_trip_over_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.sock");
        let p = path.to_str().unwrap().to_string();
        let (tx, mut rx) = mpsc::channel::<Request>(4);
        serve(&p, tx).unwrap();
        tokio::spawn(async move {
            while let Some((c, reply)) = rx.recv().await {
                let _ = reply.send(format!("got {c:?}"));
            }
        });
        assert_eq!(send(&p, "pause").await.unwrap().trim(), "got Pause");
        assert_eq!(send(&p, "nonsense").await.unwrap().trim(), HELP);
    }
}
