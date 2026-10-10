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
    read_kind_where(dir, kind, |_| true)
}

/// Rows of `kind` for which `keep` is true (parsed one line at a time).
pub(crate) fn read_kind_where(dir: &Path, kind: &str, keep: impl Fn(&Value) -> bool) -> Vec<Value> {
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
                    out.extend(
                        text.lines()
                            .filter_map(|l| serde_json::from_str(l).ok())
                            .filter(|v| keep(v)),
                    );
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
    /// an existing token reported as a create (Jupiter dates it long before)
    pub not_new: bool,
    /// checkpoints that could not be taken on time (no row for them)
    pub missed: usize,
}

/// A market cap counts only when the pool behind it holds at least this share of it in
/// liquidity. Real pools hold far more (a fresh PumpSwap pool ~40%, a curve more); a
/// quote on a few hundred dollars of liquidity can show millions.
const MIN_LIQUIDITY_SHARE: f64 = 0.01;

/// The row's market cap if liquidity backs it.
fn backed_mcap(r: &Value) -> Option<f64> {
    let m = r["mcap"].as_f64().filter(|m| *m > 0.0)?;
    (r["liquidity"].as_f64().unwrap_or(0.0) >= MIN_LIQUIDITY_SHARE * m).then_some(m)
}

impl Coin {
    fn at(&self, cp: i64, key: &str) -> Option<f64> {
        let r = self.cps.get(&cp)?;
        if key == "mcap" {
            return backed_mcap(r);
        }
        r[key].as_f64()
    }
    /// Highest (liquidity-backed) market cap observed at any checkpoint, and when.
    pub fn peak(&self) -> Option<(i64, f64)> {
        self.cps
            .iter()
            .filter_map(|(cp, r)| backed_mcap(r).map(|m| (*cp, m)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    }
    /// Highest market cap at checkpoints strictly after `cp`, as a multiple of the one at `cp`.
    pub fn forward_multiple(&self, cp: i64) -> Option<f64> {
        let base = self.at(cp, "mcap")?;
        let later = self
            .cps
            .range(cp + 1..)
            .filter_map(|(_, r)| backed_mcap(r))
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
            if r["missed"] == true {
                c.missed += 1;
                continue;
            }
            if r["stop"] == "dead" {
                c.dead_at = Some(cp);
            }
            c.not_new |= r["stop"] == "not_new";
            c.cps.insert(cp, r);
        }
    }
    // and, whatever the flags say, a coin with thousands of holders in its first minute
    // existed before its "launch"
    for c in coins.values_mut() {
        c.not_new |= c
            .cps
            .range(..=60)
            .any(|(_, r)| r["holders"].as_u64().unwrap_or(0) >= MAX_HOLDERS_FIRST_MINUTE);
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

/// More holders than this in a coin's first minute means it is not a new coin.
const MAX_HOLDERS_FIRST_MINUTE: u64 = 2_000;

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

pub(crate) fn age(ms: i64) -> String {
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
    let (coins, not_new): (Vec<Coin>, Vec<Coin>) =
        coins_of_day(dir, day).into_iter().partition(|c| !c.not_new);
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
    let missed: usize = coins.iter().map(|c| c.missed).sum();
    let _ = writeln!(o, "## Coverage");
    let _ = writeln!(
        o,
        "- launches recorded: **{n}** (PumpPortal {pp}; Jupiter only {}: other launchpads and Pump.fun coins PumpPortal missed){}",
        n - pp,
        if not_new.is_empty() {
            String::new()
        } else {
            format!("; {} existing tokens reported as creates left out", not_new.len())
        }
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
        "- checkpoints: {rows} rows on {} coins, **{:.1}% complete** ({missed} missed: due while no recorder was running); lateness median {} s, p90 {} s; {:.1}% not yet indexed by Jupiter when taken",
        tracked.len(),
        100.0 * rows as f64 / (rows + missed).max(1) as f64,
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

    // ---- after graduation (PumpSwap candles)
    let grads = read_kind_where(dir, "graduations", |r| {
        r["grad_ts"].as_i64().map(|t| day_of(t * 1000)).as_deref() == Some(day)
    });
    let amm_out = read_kind_where(dir, "amm_outcomes", |r| {
        r["grad_ts"].as_i64().map(|t| day_of(t * 1000)).as_deref() == Some(day)
    });
    let grad_mints: HashSet<String> = grads
        .iter()
        .filter_map(|r| r["mint"].as_str().map(String::from))
        .collect();
    let candles = read_kind_where(dir, "candles", |r| {
        r["mint"].as_str().is_some_and(|m| grad_mints.contains(m))
    });
    o.push_str(&post_graduation_section(&grads, &amm_out, &candles));

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

/// What happens after graduation: the pre-funded and the organic graduates, how far
/// they go within the hour, when they peak, how deep they fall on the way, and when the
/// creator sells. From the PumpSwap candles and the 1 h outcomes.
fn post_graduation_section(grads: &[Value], outcomes: &[Value], candles: &[Value]) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "\n## After graduation (PumpSwap, from every pool trade)");
    if grads.is_empty() {
        let _ = writeln!(o, "_No graduations on the tape for this day yet._");
        return o;
    }
    let instant = grads.iter().filter(|g| g["instant"] == true).count();
    let _ = writeln!(
        o,
        "- graduations followed: **{}** · pre-funded (curve filled in the create transaction): {} ({:.0}%) · organic: {}",
        grads.len(),
        instant,
        100.0 * instant as f64 / grads.len() as f64,
        grads.len() - instant
    );
    let instant_mints: HashSet<&str> = grads
        .iter()
        .filter(|g| g["instant"] == true)
        .filter_map(|g| g["mint"].as_str())
        .collect();
    // the 1 h outcome of each coin, full windows with trades only (a pool that never
    // traded is one we derived wrongly or one quoted in another token)
    let hour: Vec<&Value> = outcomes
        .iter()
        .filter(|r| {
            r["at"] == 3600 && r["full_window"] == true && r["trades"].as_u64().unwrap_or(0) > 0
        })
        .collect();
    if hour.is_empty() {
        let _ = writeln!(
            o,
            "_No full 1 h outcomes yet (they are written an hour after each graduation)._"
        );
        return o;
    }
    // deepest fall before the peak, per coin, from the candles (lows and highs by minute)
    let mut by_mint: HashMap<&str, Vec<&Value>> = HashMap::new();
    for c in candles {
        if let Some(m) = c["mint"].as_str() {
            by_mint.entry(m).or_default().push(c);
        }
    }
    let dd_before_peak = |mint: &str, peak_after_s: i64, grad_ts: i64| -> Option<f64> {
        let mut cs = by_mint.get(mint)?.clone();
        cs.sort_by_key(|c| c["minute"].as_i64().unwrap_or(0));
        let peak_minute = (grad_ts + peak_after_s).div_euclid(60) * 60;
        let (mut high, mut worst) = (0f64, 0f64);
        for c in cs {
            let minute = c["minute"].as_i64()?;
            if minute > peak_minute {
                break;
            }
            let (h, l) = (c["h"].as_f64()?, c["l"].as_f64()?);
            if high > 0.0 {
                worst = worst.max(1.0 - l / high);
            }
            high = high.max(h);
        }
        Some(worst)
    };
    let q = |v: &mut Vec<f64>, p: f64| -> String {
        if v.is_empty() {
            return "-".into();
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        format!(
            "{:.0}%",
            100.0 * v[((v.len() - 1) as f64 * p).round() as usize]
        )
    };
    let insider = |r: &Value| r["insider_share"].as_f64().unwrap_or(0.0) >= 0.1;
    let is_pre = |r: &Value| {
        r["mint"]
            .as_str()
            .is_some_and(|m| instant_mints.contains(m))
    };
    let _ = writeln!(
        o,
        "\n| within 1 h of landing | coins | alive at 1 h (≥ ½ landing) | ≥ 2× | ≥ 2.2× (≈ $100k) | ≥ 5× | ≥ 10× | time to peak (median) | creator sold | first creator sell (median min) | deepest fall before the peak, 2×+ coins (median / p75) |"
    );
    let _ = writeln!(o, "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    type Pick<'a> = (&'a str, Box<dyn Fn(&Value) -> bool + 'a>);
    let picks: Vec<Pick> = vec![
        (
            "pre-funded, insiders took ≥ 10% in the graduation slot (fabricated cap)",
            Box::new(|r| is_pre(r) && insider(r)),
        ),
        (
            "pre-funded, float left in the pool",
            Box::new(|r| is_pre(r) && !insider(r)),
        ),
        ("organic", Box::new(|r| !is_pre(r))),
        ("all", Box::new(|_| true)),
    ];
    for (label, pick) in &picks {
        let rows: Vec<&&Value> = hour.iter().filter(|r| pick(r)).collect();
        if rows.is_empty() {
            continue;
        }
        let n = rows.len() as f64;
        let share = |k: f64| {
            format!(
                "{:.0}%",
                100.0
                    * rows
                        .iter()
                        .filter(|r| r["peak_multiple"].as_f64().unwrap_or(0.0) >= k)
                        .count() as f64
                    / n
            )
        };
        let mut peaks: Vec<f64> = rows
            .iter()
            .filter_map(|r| r["peak_after_s"].as_f64())
            .collect();
        peaks.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let sold = rows
            .iter()
            .filter(|r| r["creator_sold_sol"].as_f64().unwrap_or(0.0) > 0.0)
            .count();
        let mut first_sell: Vec<f64> = rows
            .iter()
            .filter_map(|r| {
                let m = r["mint"].as_str()?;
                let mut cs = by_mint.get(m)?.clone();
                cs.sort_by_key(|c| c["minute"].as_i64().unwrap_or(0));
                cs.iter()
                    .find(|c| c["creator_sold_sol"].as_f64().unwrap_or(0.0) > 0.0)
                    .and_then(|c| c["age_min"].as_f64())
            })
            .collect();
        first_sell.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mut dds: Vec<f64> = rows
            .iter()
            .filter(|r| r["peak_multiple"].as_f64().unwrap_or(0.0) >= 2.0)
            .filter_map(|r| {
                dd_before_peak(
                    r["mint"].as_str()?,
                    r["peak_after_s"].as_i64()?,
                    r["grad_ts"].as_i64()?,
                )
            })
            .collect();
        let alive = rows.iter().filter(|r| r["alive"] == true).count();
        let _ = writeln!(
            o,
            "| {label} | {} | {} ({:.0}%) | {} | {} | {} | {} | {} | {} ({:.0}%) | {} | {} / {} |",
            rows.len(),
            alive,
            100.0 * alive as f64 / n,
            share(2.0),
            share(2.2),
            share(5.0),
            share(10.0),
            peaks
                .get(peaks.len() / 2)
                .map(|s| format!("{:.0} min", s / 60.0))
                .unwrap_or_else(|| "-".into()),
            sold,
            100.0 * sold as f64 / n,
            first_sell
                .get(first_sell.len() / 2)
                .map(|m| format!("{m:.0}"))
                .unwrap_or_else(|| "-".into()),
            q(&mut dds.clone(), 0.5),
            q(&mut dds, 0.75),
        );
    }
    let _ = writeln!(
        o,
        "_Peak multiple is against the landing price (what the first pool trade met); a pre-funded coin lands at ≈ $45k, so 2.2× ≈ $100k. 'Deepest fall before the peak' is the retracement a holder had to sit through to see the peak; it sets the trailing stop. Insider-held pools: the creator and his bundle bought 10%+ of the supply in the graduation slot, so the market cap has no float behind it. Full 1 h windows only._"
    );
    o
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
            let mut r = json!({"ts": t0 + cp * 1000, "mint": mint, "cp": cp, "mcap": mcap, "liquidity": mcap * 0.3, "holders": holders, "late_ms": 500, "traders_5m": holders});
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
        // an existing token without the flag (recorded before it existed): caught by its holders
        t.row("launches", t0, &json!({"ts": t0, "source": "pumpportal", "mint": "pumpOLD2", "launchpad": "pump.fun", "symbol": "OLDTWO"})).unwrap();
        cp("pumpOLD2", 15, 1.2e8, 131_000, json!({}));
        // a checkpoint the recorder could not take on time: counted, never used as data
        t.row("checkpoints", t0 + 90_000, &json!({"ts": t0 + 90_000, "mint": "Apump", "cp": 30, "missed": true, "late_ms": 60_000})).unwrap();
        // a Meteora coin quoted at $5M on $200 of liquidity: not a top launch
        t.row("launches", t0, &json!({"ts": t0, "created_ms": t0, "source": "jupiter", "mint": "Junk", "launchpad": "met-dbc", "symbol": "JNK"})).unwrap();
        cp("Junk", 15, 5.0e6, 30, json!({"liquidity": 200.0}));
        cp("Junk", 60, 5.1e6, 40, json!({"liquidity": 250.0}));
        // an existing token PumpPortal reported as a create: huge, flagged, left out
        t.row("launches", t0, &json!({"ts": t0, "source": "pumpportal", "mint": "pumpOLD", "launchpad": "pump.fun", "symbol": "OLD"})).unwrap();
        t.row("checkpoints", t0 + 15_000, &json!({"mint": "pumpOLD", "cp": 15, "holders": 320000, "mcap": 2.6e9, "stop": "not_new"})).unwrap();
    }

    #[test]
    fn report_from_a_small_tape() {
        let dir = tempfile::tempdir().unwrap();
        tape(dir.path());
        let day = day_of(1_791_500_000_000);
        let (text, labels) = report(dir.path(), &day);
        assert!(text.contains("launches recorded: **4**"), "{text}");
        assert!(text.contains("PumpPortal 3;"), "{text}");
        assert!(text.contains("graduated: 1"), "{text}");
        // top launch is the graduate, with its early snapshot
        assert!(
            text.contains("; 2 existing tokens reported as creates left out"),
            "{text}"
        );
        assert!(!text.contains("OLD"), "{text}");
        assert!(
            !text.contains("JNK"),
            "price-only market caps do not rank: {text}"
        );
        assert!(
            text.contains("(1 missed: due while no recorder was running)"),
            "{text}"
        );
        let top = text.split("## Top 10 launches").nth(1).unwrap();
        let first = top.lines().find(|l| l.starts_with("| 1 |")).unwrap();
        assert!(
            first.contains("CCC") && first.contains("$50.0k") && first.contains("yes"),
            "{first}"
        );
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
