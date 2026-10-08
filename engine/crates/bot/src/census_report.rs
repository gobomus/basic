//! `copybot census-report`: labels and daily tables from the census tape.
//!
//! Everything here is computed from our own recording (`copybot census`), never from
//! vendor outcome labels. Outcome columns only use checkpoints *after* the one a
//! feature comes from, so the early-signal table has no lookahead.
//!
//! Output for one UTC day: coverage, base rates, the early-signal table (holders at
//! 60 s against the chance of doubling after 60 s), the top 10 launches with how they
//! looked early, and the top 20 trending coins with the moment they first entered
//! the list. Written to `<dir>/<day>/daily.md` and per-coin `labels.jsonl`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use serde_json::{json, Value};

/// Every row of the given kind under `dir` (all day folders: a day's coins keep
/// being checkpointed into the next day).
fn read_kind(dir: &Path, kind: &str) -> Vec<Value> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().and_then(|n| n.to_str()) == Some(&format!("{kind}.jsonl")) {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    out.extend(text.lines().filter_map(|l| serde_json::from_str(l).ok()));
                }
            }
        }
    }
    out
}

fn day_of(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .format("%Y-%m-%d")
        .to_string()
}

#[derive(Debug, Default, Clone)]
pub struct Coin {
    pub mint: String,
    pub symbol: String,
    pub launchpad: String,
    pub created_ms: i64,
    pub sources: Vec<String>,
    /// checkpoint seconds → row
    pub cps: BTreeMap<i64, Value>,
    pub migrated: bool,
    pub dead_at: Option<i64>,
}

impl Coin {
    fn at(&self, cp: i64, key: &str) -> Option<f64> {
        self.cps.get(&cp).and_then(|r| r[key].as_f64())
    }
    /// Highest market cap observed at any checkpoint, and when.
    pub fn peak(&self) -> Option<(i64, f64)> {
        self.cps
            .iter()
            .filter_map(|(cp, r)| r["mcap"].as_f64().map(|m| (*cp, m)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    }
    /// Highest market cap at checkpoints strictly after `cp`, as a multiple of the one at `cp`.
    pub fn forward_multiple(&self, cp: i64) -> Option<f64> {
        let base = self.at(cp, "mcap").filter(|m| *m > 0.0)?;
        let later = self
            .cps
            .range(cp + 1..)
            .filter_map(|(_, r)| r["mcap"].as_f64())
            .fold(f64::NAN, f64::max);
        // a coin that died after `cp` simply never went higher
        Some(if later.is_nan() { 1.0 } else { later / base })
    }
    fn graduated(&self) -> bool {
        self.migrated
            || self.cps.values().any(|r| {
                r["curve_complete"] == true
                    || !r["graduated_pool"].is_null()
                    || !r["graduated_at"].is_null()
            })
    }
}

/// Coins created on `day` with their checkpoints and migration flag.
pub fn coins_of_day(dir: &Path, day: &str) -> Vec<Coin> {
    let mut coins: HashMap<String, Coin> = HashMap::new();
    for l in read_kind(dir, "launches") {
        let Some(mint) = l["mint"].as_str() else {
            continue;
        };
        let created = l["created_ms"].as_i64().or(l["ts"].as_i64()).unwrap_or(0);
        if day_of(created) != day {
            continue;
        }
        let c = coins.entry(mint.to_string()).or_insert_with(|| Coin {
            mint: mint.to_string(),
            created_ms: created,
            ..Default::default()
        });
        c.created_ms = c.created_ms.min(created);
        if c.symbol.is_empty() {
            c.symbol = l["symbol"].as_str().unwrap_or("").to_string();
        }
        if c.launchpad.is_empty() || c.launchpad == "-" {
            c.launchpad = l["launchpad"].as_str().unwrap_or("-").to_string();
        }
        if let Some(s) = l["source"].as_str() {
            if !c.sources.iter().any(|x| x == s) {
                c.sources.push(s.to_string());
            }
        }
    }
    for r in read_kind(dir, "checkpoints") {
        let (Some(mint), Some(cp)) = (r["mint"].as_str(), r["cp"].as_i64()) else {
            continue;
        };
        if let Some(c) = coins.get_mut(mint) {
            if r["stop"] == "dead" {
                c.dead_at = Some(cp);
            }
            c.cps.insert(cp, r);
        }
    }
    for m in read_kind(dir, "migrations") {
        if let Some(c) = m["mint"].as_str().and_then(|x| coins.get_mut(x)) {
            c.migrated = true;
        }
    }
    let mut v: Vec<Coin> = coins.into_values().collect();
    v.sort_by_key(|c| c.created_ms);
    v
}

/// Holders a coin must reach to count as a real launch in the top-10 table.
const MIN_HOLDERS_FOR_TOP: u64 = 20;

fn money(x: f64) -> String {
    if x >= 1e6 {
        format!("${:.2}M", x / 1e6)
    } else if x >= 1e3 {
        format!("${:.1}k", x / 1e3)
    } else {
        format!("${x:.0}")
    }
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "-".into())
}

fn age(ms: i64) -> String {
    let s = ms / 1000;
    if s < 120 {
        format!("{s}s")
    } else if s < 7200 {
        format!("{}m", s / 60)
    } else if s < 172_800 {
        format!("{:.1}h", s as f64 / 3600.0)
    } else {
        format!("{:.1}d", s as f64 / 86_400.0)
    }
}

/// The report text and the per-coin label rows.
pub fn report(dir: &Path, day: &str) -> (String, Vec<Value>) {
    let coins = coins_of_day(dir, day);
    let mut o = String::new();
    let _ = writeln!(o, "# Census {day}\n");

    // ---- coverage
    let n = coins.len();
    let pp = coins
        .iter()
        .filter(|c| c.sources.iter().any(|s| s == "pumpportal"))
        .count();
    let mut pads: BTreeMap<&str, usize> = BTreeMap::new();
    for c in &coins {
        *pads.entry(c.launchpad.as_str()).or_default() += 1;
    }
    let mut pads: Vec<_> = pads.into_iter().collect();
    pads.sort_by_key(|p| std::cmp::Reverse(p.1));
    let tracked: Vec<&Coin> = coins.iter().filter(|c| !c.cps.is_empty()).collect();
    let lates: Vec<f64> = tracked
        .iter()
        .flat_map(|c| c.cps.values())
        .filter_map(|r| r["late_ms"].as_f64())
        .collect();
    let not_found = tracked
        .iter()
        .flat_map(|c| c.cps.values())
        .filter(|r| r["found"] == false)
        .count();
    let rows: usize = tracked.iter().map(|c| c.cps.len()).sum();
    let _ = writeln!(o, "## Coverage");
    let _ = writeln!(
        o,
        "- launches recorded: **{n}** (PumpPortal {pp}; Jupiter only {}: other launchpads and Pump.fun coins PumpPortal missed)",
        n - pp
    );
    let _ = writeln!(
        o,
        "- by launchpad: {}",
        pads.iter()
            .take(8)
            .map(|(p, k)| format!("{p} {k}"))
            .collect::<Vec<_>>()
            .join(" · ")
    );
    let _ = writeln!(
        o,
        "- checkpoints: {rows} rows on {} coins; lateness median {} s, p90 {} s; {:.1}% not yet indexed by Jupiter when taken",
        tracked.len(),
        opt(engine_core::stats::median(&lates).map(|x| (x / 1000.0).round())),
        opt(engine_core::stats::quantile(&lates, 0.9).map(|x| (x / 1000.0).round())),
        100.0 * not_found as f64 / rows.max(1) as f64
    );

    // ---- base rates
    let with15: Vec<&Coin> = tracked
        .iter()
        .copied()
        .filter(|c| c.at(15, "mcap").is_some())
        .collect();
    let mult = |c: &Coin| c.forward_multiple(15).unwrap_or(1.0);
    let share = |k: f64| {
        100.0 * with15.iter().filter(|c| mult(c) >= k).count() as f64 / with15.len().max(1) as f64
    };
    let grads = coins.iter().filter(|c| c.graduated()).count();
    let _ = writeln!(o, "\n## Base rates (from the market cap at 15 s)");
    let _ = writeln!(
        o,
        "- coins with a 15 s checkpoint: {} · went on to ≥ 2x: {:.1}% · ≥ 5x: {:.1}% · ≥ 10x: {:.1}% · graduated: {grads} ({:.2}% of launches)",
        with15.len(),
        share(2.0),
        share(5.0),
        share(10.0),
        100.0 * grads as f64 / n.max(1) as f64
    );
    let dead = coins.iter().filter(|c| c.dead_at.is_some()).count();
    let _ = writeln!(
        o,
        "- declared dead by 5 min (no traders, ≤ 3 holders): {dead} ({:.0}%)",
        100.0 * dead as f64 / tracked.len().max(1) as f64
    );

    // ---- early signal: holders at 60 s vs doubling after 60 s
    let _ = writeln!(
        o,
        "\n## Early signal: holders at 60 s → chance the market cap doubles after 60 s"
    );
    let _ = writeln!(
        o,
        "| holders at 60 s | coins | doubled after | rate | lift |"
    );
    let _ = writeln!(o, "|---|---:|---:|---:|---:|");
    let at60: Vec<(f64, f64)> = tracked
        .iter()
        .filter_map(|c| Some((c.at(60, "holders")?, c.forward_multiple(60)?)))
        .collect();
    let base = at60.iter().filter(|x| x.1 >= 2.0).count() as f64 / at60.len().max(1) as f64;
    for (lo, hi, label) in [
        (0.0, 2.0, "0-1"),
        (2.0, 5.0, "2-4"),
        (5.0, 10.0, "5-9"),
        (10.0, 20.0, "10-19"),
        (20.0, 30.0, "20-29"),
        (30.0, f64::INFINITY, "30+"),
    ] {
        let b: Vec<&(f64, f64)> = at60.iter().filter(|x| x.0 >= lo && x.0 < hi).collect();
        let k = b.iter().filter(|x| x.1 >= 2.0).count();
        let rate = k as f64 / b.len().max(1) as f64;
        let _ = writeln!(
            o,
            "| {label} | {} | {k} | {:.1}% | {} |",
            b.len(),
            rate * 100.0,
            if base > 0.0 && !b.is_empty() {
                format!("{:.1}x", rate / base)
            } else {
                "-".into()
            }
        );
    }
    let _ = writeln!(
        o,
        "_All coins with a 60 s checkpoint: {} · base rate {:.1}%. Outcome uses only later checkpoints (no lookahead); one day is a small sample._",
        at60.len(),
        base * 100.0
    );

    // ---- top 10 launches
    // a launch is a coin from a launchpad that real wallets hold: new pools of old tokens
    // and one-holder tokens with a made-up market cap are left out
    let real = |c: &Coin| {
        c.launchpad != "-"
            && c.cps
                .values()
                .filter_map(|r| r["holders"].as_u64())
                .max()
                .unwrap_or(0)
                >= MIN_HOLDERS_FOR_TOP
    };
    let mut ranked: Vec<(&Coin, i64, f64)> = tracked
        .iter()
        .filter(|c| real(c))
        .filter_map(|c| c.peak().map(|(cp, m)| (*c, cp, m)))
        .collect();
    ranked.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    let _ = writeln!(
        o,
        "\n## Top 10 launches (peak market cap within 24 h) and how they looked early"
    );
    let _ = writeln!(
        o,
        "_Launchpad coins that reached {MIN_HOLDERS_FOR_TOP}+ holders; {} others (no launchpad, or never more than a few holders) left out._",
        tracked.len() - tracked.iter().filter(|c| real(c)).count()
    );
    let _ = writeln!(
        o,
        "| # | coin | launchpad | peak | at | graduated | mcap 15s / 60s / 5m | holders 15s / 60s / 5m | traders 5m @60s | top10 % @60s | dev % @60s |"
    );
    let _ = writeln!(o, "|---:|---|---|---:|---:|---|---|---|---:|---:|---:|");
    for (i, (c, cp, m)) in ranked.iter().take(10).enumerate() {
        let mc = |s: i64| c.at(s, "mcap").map(money).unwrap_or_else(|| "-".into());
        let h = |s: i64| opt(c.at(s, "holders").map(|x| x as u64));
        let _ = writeln!(
            o,
            "| {} | {} `{}…` | {} | {} | {} | {} | {} / {} / {} | {} / {} / {} | {} | {} | {} |",
            i + 1,
            c.symbol,
            &c.mint[..6.min(c.mint.len())],
            c.launchpad,
            money(*m),
            age(cp * 1000),
            if c.graduated() { "yes" } else { "no" },
            mc(15),
            mc(60),
            mc(300),
            h(15),
            h(60),
            h(300),
            opt(c.at(60, "traders_5m").map(|x| x as u64)),
            opt(c.at(60, "top_holders_pct").map(|x| format!("{x:.0}"))),
            opt(c.at(60, "dev_balance_pct").map(|x| format!("{x:.1}"))),
        );
    }

    // ---- top 20 trending
    let trending: Vec<Value> = read_kind(dir, "trending")
        .into_iter()
        .filter(|r| r["ts"].as_i64().map(day_of).as_deref() == Some(day))
        .collect();
    #[derive(Default)]
    struct T {
        symbol: String,
        launchpad: String,
        created_ms: Option<i64>,
        first_ts: i64,
        best_rank: u64,
        captures: usize,
        first_mcap: Option<f64>,
        peak_mcap: f64,
        first_holders: Option<u64>,
        lists: HashSet<String>,
    }
    let mut t: HashMap<String, T> = HashMap::new();
    for r in &trending {
        let (Some(mint), Some(list), Some(rank), Some(ts)) = (
            r["mint"].as_str(),
            r["list"].as_str(),
            r["rank"].as_u64(),
            r["ts"].as_i64(),
        ) else {
            continue;
        };
        if let Ok(pk) = mint.parse() {
            if chain::base_assets::is_base_asset(&pk) {
                continue;
            }
        }
        let e = t.entry(mint.to_string()).or_default();
        e.lists.insert(list.to_string());
        if let Some(m) = r["mcap"].as_f64() {
            e.peak_mcap = e.peak_mcap.max(m);
        }
        if list != "jup_trending_1h" || rank > 20 {
            continue;
        }
        if e.captures == 0 || ts < e.first_ts {
            e.first_ts = ts;
            e.first_mcap = r["mcap"].as_f64();
            e.first_holders = r["holders"].as_u64();
        }
        e.captures += 1;
        e.best_rank = if e.best_rank == 0 {
            rank
        } else {
            e.best_rank.min(rank)
        };
        if e.symbol.is_empty() {
            e.symbol = r["symbol"].as_str().unwrap_or("").to_string();
        }
        if let Some(lp) = r["launchpad"].as_str() {
            e.launchpad = lp.to_string();
        }
        if e.created_ms.is_none() {
            e.created_ms = r["created_ms"].as_i64();
        }
    }
    let mut tr: Vec<(&String, &T)> = t
        .iter()
        .filter(|(_, x)| x.captures > 0 && !x.launchpad.is_empty())
        .collect();
    tr.sort_by(|a, b| {
        b.1.captures
            .cmp(&a.1.captures)
            .then(a.1.best_rank.cmp(&b.1.best_rank))
    });
    let _ = writeln!(
        o,
        "\n## Top 20 trending memecoins (Jupiter 1 h trending, top 20), and the moment each first entered"
    );
    let _ = writeln!(
        o,
        "| # | coin | launchpad | times in top 20 | best rank | first entered (UTC) | age then | mcap then | peak mcap today | holders then | launch on tape | also in |"
    );
    let _ = writeln!(
        o,
        "|---:|---|---|---:|---:|---|---:|---:|---:|---:|---|---|"
    );
    let on_tape: HashSet<String> = read_kind(dir, "launches")
        .iter()
        .filter_map(|l| l["mint"].as_str().map(String::from))
        .collect();
    for (i, (mint, x)) in tr.iter().take(20).enumerate() {
        let others: Vec<&str> = x
            .lists
            .iter()
            .filter(|l| l.as_str() != "jup_trending_1h")
            .map(|l| l.as_str())
            .collect();
        let _ = writeln!(
            o,
            "| {} | {} `{}…` | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            i + 1,
            x.symbol,
            &mint[..6.min(mint.len())],
            x.launchpad,
            x.captures,
            x.best_rank,
            chrono::DateTime::from_timestamp_millis(x.first_ts)
                .map(|d| d.format("%H:%M").to_string())
                .unwrap_or_default(),
            x.created_ms
                .map(|c| age(x.first_ts - c))
                .unwrap_or_else(|| "-".into()),
            x.first_mcap.map(money).unwrap_or_else(|| "-".into()),
            money(x.peak_mcap),
            opt(x.first_holders),
            if on_tape.contains(mint.as_str()) {
                "yes"
            } else {
                "no"
            },
            others.join(", ")
        );
    }

    // ---- labels
    let labels: Vec<Value> = tracked
        .iter()
        .map(|c| {
            let peak = c.peak();
            json!({
                "mint": c.mint, "day": day, "launchpad": c.launchpad, "created_ms": c.created_ms,
                "sources": c.sources, "graduated": c.graduated(), "dead_at": c.dead_at,
                "peak_mcap": peak.map(|p| p.1), "peak_cp": peak.map(|p| p.0),
                "mcap_15": c.at(15, "mcap"), "mcap_60": c.at(60, "mcap"), "mcap_300": c.at(300, "mcap"),
                "holders_15": c.at(15, "holders"), "holders_60": c.at(60, "holders"), "holders_300": c.at(300, "holders"),
                "fwd_mult_after_15": c.forward_multiple(15), "fwd_mult_after_60": c.forward_multiple(60),
                "fwd_mult_after_300": c.forward_multiple(300),
            })
        })
        .collect();
    (o, labels)
}

pub fn write(dir: &Path, day: &str) -> anyhow::Result<String> {
    let (text, labels) = report(dir, day);
    let d = dir.join(day);
    std::fs::create_dir_all(&d)?;
    std::fs::write(d.join("daily.md"), &text)?;
    let mut l = String::new();
    for row in labels {
        l.push_str(&row.to_string());
        l.push('\n');
    }
    std::fs::write(d.join("labels.jsonl"), l)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tape(dir: &Path) {
        let t = crate::census::Tape::new(dir);
        // 2026-10-08 ~ 22:13 UTC.
        // A: 30 holders at 60 s and runs 4x after; B: 2 holders and dies; C: graduates
        let t0 = 1_791_500_000_000i64;
        for (mint, sym) in [("Apump", "AAA"), ("Bpump", "BBB"), ("Cpump", "CCC")] {
            t.row("launches", t0, &json!({"ts": t0, "source": "pumpportal", "mint": mint, "launchpad": "pump.fun", "symbol": sym})).unwrap();
        }
        t.row("launches", t0, &json!({"ts": t0 + 5000, "created_ms": t0, "source": "jupiter", "mint": "Apump", "launchpad": "pump.fun"})).unwrap();
        let cp = |mint: &str, cp: i64, mcap: f64, holders: u64, extra: Value| {
            let mut r = json!({"ts": t0 + cp * 1000, "mint": mint, "cp": cp, "mcap": mcap, "holders": holders, "late_ms": 500, "traders_5m": holders});
            for (k, v) in extra.as_object().unwrap() {
                r[k] = v.clone();
            }
            t.row("checkpoints", t0 + cp * 1000, &r).unwrap();
        };
        cp("Apump", 15, 4000.0, 10, json!({}));
        cp("Apump", 60, 5000.0, 30, json!({"top_holders_pct": 30.0}));
        cp("Apump", 300, 20000.0, 80, json!({}));
        cp("Bpump", 15, 3100.0, 2, json!({}));
        cp("Bpump", 60, 3000.0, 2, json!({}));
        cp(
            "Bpump",
            300,
            3000.0,
            1,
            json!({"stop": "dead", "traders_5m": 0}),
        );
        cp("Cpump", 15, 9000.0, 25, json!({}));
        cp("Cpump", 60, 30000.0, 40, json!({}));
        cp("Cpump", 300, 50000.0, 120, json!({"curve_complete": true}));
        t.row(
            "migrations",
            t0 + 300_000,
            &json!({"ts": t0 + 300_000, "mint": "Cpump"}),
        )
        .unwrap();
        for (i, (mint, rank)) in [("Xpump", 1u64), ("Ypump", 2)].iter().enumerate() {
            for k in 0..(3 - i as i64) {
                t.row("trending", t0 + k * 300_000, &json!({"ts": t0 + k * 300_000, "list": "jup_trending_1h", "rank": rank, "mint": mint, "symbol": "T", "launchpad": "pump.fun", "mcap": 100000.0 * (k + 1) as f64, "holders": 500, "created_ms": t0 - 3_600_000})).unwrap();
            }
        }
        // a stablecoin in a trending list is not a memecoin
        t.row("trending", t0, &json!({"ts": t0, "list": "jup_trending_1h", "rank": 3, "mint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "launchpad": "-"})).unwrap();
    }

    #[test]
    fn report_from_a_small_tape() {
        let dir = tempfile::tempdir().unwrap();
        tape(dir.path());
        let day = day_of(1_791_500_000_000);
        let (text, labels) = report(dir.path(), &day);
        assert!(text.contains("launches recorded: **3**"), "{text}");
        assert!(text.contains("PumpPortal 3;"), "{text}");
        assert!(text.contains("graduated: 1"), "{text}");
        // top launch is the graduate, with its early snapshot
        let top = text.split("## Top 10 launches").nth(1).unwrap();
        let first = top.lines().find(|l| l.starts_with("| 1 |")).unwrap();
        assert!(
            first.contains("CCC") && first.contains("$50.0k") && first.contains("yes"),
            "{first}"
        );
        // early signal: A (30 holders) and C (40) are in 30+, only A doubled after 60 s;
        // B (2 holders) did not
        assert!(text.contains("| 30+ | 2 | 1 | 50.0% | 1.5x |"), "{text}");
        assert!(text.contains("| 2-4 | 1 | 0 | 0.0% | 0.0x |"), "{text}");
        // trending: X entered first and stayed longest; the stablecoin is filtered out
        let tr = text.split("## Top 20 trending").nth(1).unwrap();
        let first = tr.lines().find(|l| l.starts_with("| 1 |")).unwrap();
        assert!(
            first.contains("Xpump") && first.contains("| 3 |"),
            "{first}"
        );
        assert!(!tr.contains("EPjFWd"));
        // labels: forward multiples from later checkpoints only
        let a = labels.iter().find(|l| l["mint"] == "Apump").unwrap();
        assert_eq!(a["fwd_mult_after_60"], 4.0);
        assert_eq!(a["sources"].as_array().unwrap().len(), 2);
        let b = labels.iter().find(|l| l["mint"] == "Bpump").unwrap();
        assert_eq!(b["dead_at"], 300);
        assert_eq!(
            b["fwd_mult_after_300"], 1.0,
            "nothing after the last checkpoint"
        );
    }
}
