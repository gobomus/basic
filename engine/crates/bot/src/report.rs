//! `copybot report`: the profit proof.
//!
//! Reads the JSONL journal and answers, after every cost the engine models:
//! did copying make money, which leaders and exit policies carried it, and
//! what did execution delay cost? Works on paper, shadow and live journals.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use engine_core::stats::{mean, median, profit_factor, quantile};
use serde_json::Value;

/// Fewer closed trades than this prove nothing either way.
const MIN_TRADES: usize = 30;
/// Go / no-go gate (docs/07-roadmap.md, Gate 1) before risking money.
const GATE_SIGNALS: usize = 300;
const GATE_TRADES: usize = 100;
const GATE_LEADERS: usize = 3;
/// A leader counts as individually positive only with at least this many trades.
const GATE_LEADER_TRADES: usize = 5;

#[derive(Default)]
struct Trade {
    leader: String,
    policy: String,
    reason: String,
    pnl: f64,
    ret: f64,
    held_ms: i64,
    peak: f64,
}

#[derive(Default)]
struct Agg {
    n: usize,
    wins: usize,
    pnl: f64,
    rets: Vec<f64>,
}

impl Agg {
    fn add(&mut self, pnl: f64, ret: f64) {
        self.n += 1;
        self.wins += usize::from(pnl > 0.0);
        self.pnl += pnl;
        self.rets.push(ret);
    }
    fn win_rate(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.wins as f64 / self.n as f64
        }
    }
}

/// Every `*.jsonl` under `dir`, subfolders included (several downloaded runs can sit side by side).
fn journal_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> anyhow::Result<()> {
    for e in std::fs::read_dir(dir).map_err(|e| anyhow::anyhow!("{}: {e}", dir.display()))? {
        let p = e?.path();
        if p.is_dir() {
            journal_files(&p, out)?;
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
    Ok(())
}

fn read_journal(dir: &str, since_ms: Option<i64>) -> anyhow::Result<Vec<Value>> {
    let mut files = Vec::new();
    journal_files(std::path::Path::new(dir), &mut files)?;
    files.sort();
    let mut out = Vec::new();
    for f in files {
        for line in std::fs::read_to_string(&f)?.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if since_ms.is_some_and(|s| v["ts"].as_i64().unwrap_or(0) < s) {
                continue;
            }
            out.push(v);
        }
    }
    // runs from different folders interleave by time
    out.sort_by_key(|v| v["ts"].as_i64().unwrap_or(0));
    Ok(out)
}

/// 95% bootstrap confidence interval of the mean (deterministic).
fn bootstrap_ci(xs: &[f64]) -> Option<(f64, f64)> {
    use rand::{rngs::StdRng, Rng, SeedableRng};
    if xs.len() < 2 {
        return None;
    }
    let mut rng = StdRng::seed_from_u64(42);
    let mut means: Vec<f64> = (0..4000)
        .map(|_| {
            (0..xs.len())
                .map(|_| xs[rng.gen_range(0..xs.len())])
                .sum::<f64>()
                / xs.len() as f64
        })
        .collect();
    means.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    Some((means[100], means[3899]))
}

fn pct(x: f64) -> String {
    format!("{:+.1}%", x * 100.0)
}

fn sol(x: f64) -> String {
    format!("{x:+.4}")
}

fn short(a: &str) -> String {
    if a.len() > 10 {
        format!("{}…{}", &a[..4], &a[a.len() - 4..])
    } else {
        a.to_string()
    }
}

/// Build the report text from the journal in `dir` (optionally only the last `hours`).
pub fn run(dir: &str, hours: Option<f64>) -> anyhow::Result<String> {
    let since = hours.map(|h| crate::journal::now_ms() - (h * 3_600_000.0) as i64);
    let recs = read_journal(dir, since)?;
    anyhow::ensure!(!recs.is_empty(), "no journal records in {dir}");

    let mut labels: HashMap<String, String> = HashMap::new();
    let mut policy_of: HashMap<String, String> = HashMap::new();
    let mut trades: Vec<Trade> = vec![];
    let (mut signals, mut copies) = (0usize, 0usize);
    let mut skips: BTreeMap<String, usize> = BTreeMap::new();
    let mut slot_lags: Vec<f64> = vec![];
    let mut decision_ms: Vec<f64> = vec![];
    let mut fetch_ms: Vec<f64> = vec![];
    let (mut fills_ok, mut fills_missed) = (0usize, 0usize);
    let (mut slip_bps, mut latency): (Vec<f64>, Vec<f64>) = (vec![], vec![]);
    let mut shadows: BTreeMap<String, Agg> = BTreeMap::new();
    let mut shadow_positions = 0usize;
    let mut live_policy_pnl: BTreeMap<String, Agg> = BTreeMap::new();
    let mut feed_drops = 0usize;
    let (mut first, mut last) = (i64::MAX, 0i64);
    let mut modes: BTreeMap<String, usize> = BTreeMap::new();

    for r in &recs {
        let ts = r["ts"].as_i64().unwrap_or(0);
        first = first.min(ts);
        last = last.max(ts);
        match r["kind"].as_str().unwrap_or("") {
            "signal" => {
                if r["side"] != "buy" {
                    continue;
                }
                signals += 1;
                if let (Some(l), Some(lab)) = (r["leader"].as_str(), r["label"].as_str()) {
                    if !lab.is_empty() {
                        labels.insert(l.to_string(), lab.to_string());
                    }
                }
                if r["decision"] == "buy" {
                    copies += 1;
                } else if let Some(reason) = r["skip_reason"].as_str() {
                    *skips.entry(reason.to_string()).or_default() += 1;
                }
                if let Some(l) = r["slot_lag"].as_f64() {
                    slot_lags.push(l);
                }
                if let Some(d) = r["decision_ms"].as_f64() {
                    decision_ms.push(d);
                }
                if let Some(d) = r["fetch_ms"].as_f64() {
                    fetch_ms.push(d);
                }
            }
            "feed" if r["connected"] == false => feed_drops += 1,
            "position_open" => {
                if let (Some(m), Some(p)) = (r["mint"].as_str(), r["exit_policy"].as_str()) {
                    policy_of.insert(m.to_string(), p.to_string());
                }
                if let Some(m) = r["mode"].as_str() {
                    *modes.entry(m.to_string()).or_default() += 1;
                }
            }
            "fill" if r["side"] == "buy" && r["simulated"] == true => {
                if r["failed"].is_null() {
                    fills_ok += 1;
                    if let Some(b) = r["slippage_bps"].as_f64() {
                        slip_bps.push(b);
                    }
                    if let Some(l) = r["latency_ms"].as_f64() {
                        latency.push(l);
                    }
                } else {
                    fills_missed += 1;
                }
            }
            "position_exit" => {
                let mint = r["mint"].as_str().unwrap_or_default();
                let t = Trade {
                    leader: r["leader"].as_str().unwrap_or_default().to_string(),
                    policy: policy_of.get(mint).cloned().unwrap_or_default(),
                    reason: r["exit_reason"].as_str().unwrap_or("").to_string(),
                    pnl: r["pnl_sol"].as_f64().unwrap_or(0.0),
                    ret: r["ret"].as_f64().unwrap_or(0.0),
                    held_ms: r["held_ms"].as_i64().unwrap_or(0),
                    peak: r["max_mult"].as_f64().unwrap_or(1.0),
                };
                live_policy_pnl
                    .entry(t.policy.clone())
                    .or_default()
                    .add(t.pnl, t.ret);
                trades.push(t);
            }
            "position_close" => {
                shadow_positions += 1;
                let size = r["cost_lamports"].as_f64().unwrap_or(0.0) / 1e9;
                for s in r["shadows"].as_array().into_iter().flatten() {
                    if let Some(name) = s["policy"].as_str() {
                        let pnl = s["pnl_sol"].as_f64().unwrap_or(0.0);
                        let ret =
                            s["ret"]
                                .as_f64()
                                .unwrap_or(if size > 0.0 { pnl / size } else { 0.0 });
                        shadows.entry(name.to_string()).or_default().add(pnl, ret);
                    }
                }
            }
            _ => {}
        }
    }

    let mut o = String::new();
    let span_h = (last - first).max(0) as f64 / 3_600_000.0;
    let modes_s = modes
        .iter()
        .map(|(m, n)| format!("{m}:{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    writeln!(
        o,
        "COPYBOT REPORT · {span_h:.1} h of journal · positions opened by mode: {modes_s}"
    )?;
    writeln!(o, "{}", "=".repeat(78))?;

    // ---- funnel
    writeln!(o, "\nSIGNALS")?;
    writeln!(
        o,
        "  leader buys seen {signals} · copied {copies} · skipped {}",
        signals - copies
    )?;
    if !skips.is_empty() {
        let mut v: Vec<_> = skips.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        let s: Vec<String> = v.iter().take(6).map(|(k, n)| format!("{k} {n}")).collect();
        writeln!(o, "  top skip reasons: {}", s.join(" · "))?;
    }
    if feed_drops > 0 {
        writeln!(
            o,
            "  data feed dropped {feed_drops} time(s): leader trades during those gaps were missed"
        )?;
    }
    if let Some(m) = median(&decision_ms) {
        writeln!(
            o,
            "  our pipeline: first sign of the leader's trade to decision, median {m:.0} ms · p90 {:.0} ms (the number to cut when optimising latency)",
            quantile(&decision_ms, 0.9).unwrap_or(0.0)
        )?;
    }
    if let Some(m) = median(&fetch_ms) {
        writeln!(
            o,
            "    of which fetching the transaction from the RPC: median {m:.0} ms · p90 {:.0} ms",
            quantile(&fetch_ms, 0.9).unwrap_or(0.0)
        )?;
    }
    if let Some(m) = median(&slot_lags) {
        writeln!(
            o,
            "  detection lag: median {m:.0} slots (~{:.1} s)",
            m * 0.4
        )?;
    }
    if fills_ok + fills_missed > 0 {
        writeln!(
            o,
            "  simulated entries: {fills_ok} filled · {fills_missed} missed (price ran past slippage by landing)"
        )?;
        if let (Some(sl), Some(la)) = (median(&slip_bps), median(&latency)) {
            writeln!(
                o,
                "  fill vs the leader-time quote: median {sl:+.0} bps worse · landing delay {la:.0} ms"
            )?;
        }
    }

    // ---- results
    writeln!(o, "\nRESULTS (closed positions, after fees)")?;
    let pnls: Vec<f64> = trades.iter().map(|t| t.pnl).collect();
    let rets: Vec<f64> = trades.iter().map(|t| t.ret).collect();
    let total: f64 = pnls.iter().sum();
    if trades.is_empty() {
        writeln!(o, "  no closed positions yet")?;
    } else {
        let wins = trades.iter().filter(|t| t.pnl > 0.0).count();
        writeln!(
            o,
            "  trades {} · win rate {:.0}% · total PnL {} SOL",
            trades.len(),
            wins as f64 / trades.len() as f64 * 100.0,
            sol(total)
        )?;
        writeln!(
            o,
            "  per trade: mean {} · median {} · expectancy {} SOL",
            pct(mean(&rets).unwrap_or(0.0)),
            pct(median(&rets).unwrap_or(0.0)),
            sol(total / trades.len() as f64)
        )?;
        if let Some((lo, hi)) = bootstrap_ci(&rets) {
            writeln!(
                o,
                "  mean return per trade, 95% confidence interval: {} to {}",
                pct(lo),
                pct(hi)
            )?;
        }
        writeln!(
            o,
            "  profit factor {} · avg hold {:.1} min · avg peak {:.2}x (what a perfect exit would have seen)",
            profit_factor(&pnls).map_or("∞ (no losers)".to_string(), |p| format!("{p:.2}")),
            trades.iter().map(|t| t.held_ms as f64).sum::<f64>() / trades.len() as f64 / 60_000.0,
            trades.iter().map(|t| t.peak).sum::<f64>() / trades.len() as f64,
        )?;
        let best = trades.iter().map(|t| t.ret).fold(f64::MIN, f64::max);
        let worst = trades.iter().map(|t| t.ret).fold(f64::MAX, f64::min);
        writeln!(o, "  best {} · worst {}", pct(best), pct(worst))?;
    }

    // ---- by leader
    if !trades.is_empty() {
        let mut by_leader: BTreeMap<String, Agg> = BTreeMap::new();
        for t in &trades {
            by_leader
                .entry(t.leader.clone())
                .or_default()
                .add(t.pnl, t.ret);
        }
        writeln!(o, "\nBY LEADER")?;
        writeln!(
            o,
            "  {:<22} {:>6} {:>6} {:>12} {:>10}",
            "leader", "trades", "win%", "PnL SOL", "avg ret"
        )?;
        let mut rows: Vec<_> = by_leader.iter().collect();
        rows.sort_by(|a, b| {
            b.1.pnl
                .partial_cmp(&a.1.pnl)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (l, a) in rows {
            let name = labels.get(l).cloned().unwrap_or_else(|| short(l));
            writeln!(
                o,
                "  {:<22} {:>6} {:>5.0}% {:>12} {:>10}",
                name,
                a.n,
                a.win_rate() * 100.0,
                sol(a.pnl),
                pct(mean(&a.rets).unwrap_or(0.0))
            )?;
        }
    }

    // ---- why positions closed
    if trades.iter().any(|t| !t.reason.is_empty()) {
        let mut by_reason: BTreeMap<&str, Agg> = BTreeMap::new();
        for t in &trades {
            by_reason
                .entry(if t.reason.is_empty() {
                    "(unknown)"
                } else {
                    &t.reason
                })
                .or_default()
                .add(t.pnl, t.ret);
        }
        writeln!(o, "\nBY EXIT REASON (which rule closed the position)")?;
        let mut rows: Vec<_> = by_reason.iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1.n));
        for (r, a) in rows {
            writeln!(
                o,
                "  {:<14} trades {:>3} · win {:>3.0}% · PnL {} SOL · avg {}",
                r,
                a.n,
                a.win_rate() * 100.0,
                sol(a.pnl),
                pct(mean(&a.rets).unwrap_or(0.0))
            )?;
        }
    }

    // ---- exits actually used
    if live_policy_pnl.len() > 1 || live_policy_pnl.keys().any(|k| !k.is_empty()) {
        writeln!(o, "\nEXIT POLICY IN USE")?;
        for (p, a) in &live_policy_pnl {
            writeln!(
                o,
                "  {:<18} trades {:>3} · win {:>3.0}% · PnL {} SOL · avg {}",
                if p.is_empty() { "(unknown)" } else { p },
                a.n,
                a.win_rate() * 100.0,
                sol(a.pnl),
                pct(mean(&a.rets).unwrap_or(0.0))
            )?;
        }
    }

    // ---- alternative exits (same entries, same price path)
    if !shadows.is_empty() {
        writeln!(
            o,
            "\nALTERNATIVE EXITS: every policy replayed on the same {shadow_positions} entries and price paths"
        )?;
        writeln!(
            o,
            "  {:<18} {:>6} {:>6} {:>12} {:>10}",
            "policy", "n", "win%", "PnL SOL", "avg ret"
        )?;
        let mut rows: Vec<_> = shadows.iter().collect();
        rows.sort_by(|a, b| {
            b.1.pnl
                .partial_cmp(&a.1.pnl)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (p, a) in rows {
            writeln!(
                o,
                "  {:<18} {:>6} {:>5.0}% {:>12} {:>10}",
                p,
                a.n,
                a.win_rate() * 100.0,
                sol(a.pnl),
                pct(mean(&a.rets).unwrap_or(0.0))
            )?;
        }
        writeln!(
            o,
            "  (scored when positions are finalised: after [infra.paper] afterlife_secs, or at `stop`)"
        )?;
    }

    // ---- verdict
    writeln!(o, "\nGO / NO-GO CHECKLIST (before any real money)")?;
    let n = trades.len();
    let ci = bootstrap_ci(&rets);
    let mut by_leader_pnl: BTreeMap<&str, (usize, f64)> = BTreeMap::new();
    for t in &trades {
        let e = by_leader_pnl.entry(t.leader.as_str()).or_default();
        e.0 += 1;
        e.1 += t.pnl;
    }
    let positive_leaders = by_leader_pnl
        .values()
        .filter(|(c, p)| *c >= GATE_LEADER_TRADES && *p > 0.0)
        .count();
    let mark = |ok: bool| if ok { "[x]" } else { "[ ]" };
    writeln!(
        o,
        "  {} {GATE_SIGNALS}+ leader buys seen ({signals})",
        mark(signals >= GATE_SIGNALS)
    )?;
    writeln!(
        o,
        "  {} {GATE_TRADES}+ closed trades ({n})",
        mark(n >= GATE_TRADES)
    )?;
    writeln!(
        o,
        "  {} mean return per trade is positive with 95% confidence{}",
        mark(ci.is_some_and(|(lo, _)| lo > 0.0)),
        ci.map_or(String::new(), |(lo, hi)| format!(
            " ({} to {})",
            pct(lo),
            pct(hi)
        ))
    )?;
    writeln!(
        o,
        "  {} at least {GATE_LEADERS} leaders individually in profit with {GATE_LEADER_TRADES}+ trades ({positive_leaders})",
        mark(positive_leaders >= GATE_LEADERS)
    )?;

    writeln!(o, "\nVERDICT")?;
    if n < MIN_TRADES {
        writeln!(
            o,
            "  Too early: {n} closed trades, need at least {MIN_TRADES} before any conclusion. Keep running."
        )?;
    } else {
        match ci {
            Some((lo, _)) if lo > 0.0 => writeln!(
                o,
                "  Edge likely: the average trade is positive after costs, with statistical confidence ({} SOL over {n} trades). Tick the remaining boxes above before going live.",
                sol(total)
            )?,
            Some((_, hi)) if hi < 0.0 => writeln!(
                o,
                "  Losing after costs over {n} trades ({} SOL). Change leaders or exits (see the tables) before risking money.",
                sol(total)
            )?,
            _ => writeln!(
                o,
                "  Inconclusive over {n} trades ({} SOL): the results are still consistent with no edge. Keep running.",
                sol(total)
            )?,
        }
    }
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(dir: &std::path::Path, recs: &[Value]) {
        let body: String = recs.iter().map(|r| r.to_string() + "\n").collect();
        std::fs::write(dir.join("2026-10-02.jsonl"), body).unwrap();
    }

    #[test]
    fn summarises_trades_leaders_fills_and_alternative_exits() {
        let dir = tempfile::tempdir().unwrap();
        let mut recs = vec![
            json!({"kind":"signal","ts":1_000,"side":"buy","leader":"LEADERaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","label":"alpha","decision":"buy","slot_lag":4}),
            json!({"kind":"signal","ts":1_100,"side":"buy","leader":"LEADERaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","label":"alpha","decision":"skip","skip_reason":"PriceAlreadyMoved","slot_lag":6}),
            json!({"kind":"position_open","ts":1_200,"mint":"M1","mode":"paper","exit_policy":"fast_scalp"}),
            json!({"kind":"fill","ts":1_300,"side":"buy","simulated":true,"slippage_bps":120.0,"latency_ms":1000}),
            json!({"kind":"fill","ts":1_310,"side":"buy","simulated":true,"failed":"slippage"}),
            json!({"kind":"position_exit","ts":2_000,"mint":"M1","leader":"LEADERaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","pnl_sol":0.02,"ret":0.2,"held_ms":120000,"max_mult":1.5,"exit_reason":"TakeProfit"}),
            json!({"kind":"position_close","ts":3_000,"mint":"M1","cost_lamports":100_000_000u64,"shadows":[
                {"policy":"fast_scalp","pnl_sol":0.02,"ret":0.2},{"policy":"moonbag","pnl_sol":-0.05,"ret":-0.5}]}),
        ];
        write(dir.path(), &recs);
        let out = run(dir.path().to_str().unwrap(), None).unwrap();
        assert!(
            out.contains("leader buys seen 2 · copied 1 · skipped 1"),
            "{out}"
        );
        assert!(out.contains("PriceAlreadyMoved 1"));
        assert!(out.contains("1 filled · 1 missed"));
        assert!(out.contains("trades 1 · win rate 100%"));
        assert!(out.contains("alpha"));
        assert!(out.contains("fast_scalp"));
        assert!(
            out.contains("BY EXIT REASON") && out.contains("TakeProfit"),
            "{out}"
        );
        assert!(out.contains("Too early: 1 closed trades"));
        assert!(out.contains("[ ] 300+ leader buys seen (2)"));
        // the better alternative is listed first
        let alt = out.split("ALTERNATIVE EXITS").nth(1).unwrap();
        assert!(alt.find("fast_scalp").unwrap() < alt.find("moonbag").unwrap());

        // 40 losing trades: not profitable
        recs.clear();
        for i in 0..40 {
            recs.push(json!({"kind":"position_exit","ts":10+i,"mint":format!("X{i}"),"leader":"L","pnl_sol":-0.01,"ret":-0.1,"held_ms":1000,"max_mult":1.0}));
        }
        write(dir.path(), &recs);
        let out = run(dir.path().to_str().unwrap(), None).unwrap();
        assert!(out.contains("Losing after costs over 40 trades"), "{out}");
        assert!(
            out.contains("confidence interval: -10.0% to -10.0%"),
            "{out}"
        );

        // 60 winners with some spread: edge likely, checklist ticks
        recs.clear();
        for i in 0..60 {
            let r = 0.05 + (i % 5) as f64 * 0.01;
            recs.push(json!({"kind":"position_exit","ts":10+i,"mint":format!("W{i}"),"leader":format!("LEAD{}", i % 4),"pnl_sol":r * 0.1,"ret":r,"held_ms":1000,"max_mult":1.2}));
        }
        write(dir.path(), &recs);
        let out = run(dir.path().to_str().unwrap(), None).unwrap();
        assert!(out.contains("Edge likely"), "{out}");
        assert!(
            out.contains("[x] mean return per trade is positive with 95% confidence"),
            "{out}"
        );
        assert!(
            out.contains("[x] at least 3 leaders individually in profit"),
            "{out}"
        );
        assert!(out.contains("[ ] 100+ closed trades (60)"), "{out}");
    }

    #[test]
    fn empty_journal_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run(dir.path().to_str().unwrap(), None).is_err());
    }
}
