//! The live engine: one event loop owning all trading state.
//!
//! Inputs : Geyser/deshred transactions, slot ticks, a 1 s clock, order
//!          results from execution tasks, operator commands (control socket).
//! Outputs: orders (live) or simulated fills (shadow/paper), journal records,
//!          log events, and feed filter updates (leaders + held mints).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chain::detect::{self, DetectedSwap, Template};
use chain::geyser::{FeedEvent, Filters};
use chain::model::ChainTx;
use chain::pump::{self, PumpEvent};
use chain::pump_amm;
use chain::solana_sdk::pubkey::Pubkey;
use engine_core::config::{LeaderConfig, RunMode, SizingConfig};
use engine_core::exit::{ExitAction, ExitPolicy, ExitReason, MarketEvent, PositionState};
use engine_core::replay::{self, CostModel, PathInput};
use engine_core::sizing::{self, BookContext, EntryContext, SizeDecision, SkipReason};
use engine_core::types::{lamports_to_sol, sol_to_lamports, FeedSource, Side, Venue};
use serde_json::json;
use tokio::sync::{mpsc, watch};

use crate::cfg::BotConfig;
use crate::control::{Command, Request};
use crate::exec::{Confirmation, Exec, SendOutcome, SOL_MINT};
use crate::gmgn::{Gmgn, TokenIntel};
use crate::journal::{now_ms, Journal};

const PATH_CAP: usize = 200_000;
/// Every Pump.fun mint has a fixed supply of 1B tokens.
const PUMP_SUPPLY: f64 = 1e9;

/// Latest known price for the token, else the leader's fill.
fn entry_price(t: &Option<TokenInfo>, s: &DetectedSwap) -> f64 {
    t.as_ref()
        .map(|t| t.price)
        .filter(|p| *p > 0.0)
        .unwrap_or(s.price_sol)
}

/// Market cap in SOL where the supply is fixed and known.
fn market_cap_sol(venue: Venue, price_sol: f64) -> Option<f64> {
    matches!(venue, Venue::PumpFunCurve | Venue::PumpSwap)
        .then_some(price_sol * PUMP_SUPPLY)
        .filter(|m| *m > 0.0)
}
const AFTERLIFE_MS: i64 = 2 * 3600 * 1000;

#[derive(Debug, Clone)]
struct TokenInfo {
    template: Template,
    price: f64,
    pool_sol: Option<u64>,
    creator: Option<Pubkey>,
    token_program: Pubkey,
    decimals: u8,
    created_ms: Option<i64>,
    migrated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PosStatus {
    Opening,
    Open,
}

#[derive(Debug, Clone)]
struct PendingSell {
    signature: String,
}

#[derive(Debug, Clone)]
struct Position {
    mint: Pubkey,
    leader: Pubkey,
    leader_label: String,
    opened_ms: i64,
    policy_name: String,
    policy: ExitPolicy,
    policy_hash: String,
    state: PositionState,
    path: Vec<MarketEvent>,
    cost_lamports: u64,
    /// Planned spend of in-flight buys (counts toward exposure until filled).
    pending_cost: u64,
    fees_lamports: u64,
    proceeds_lamports: u64,
    initial_tokens: u64,
    held_tokens: u64,
    decimals: u8,
    token_program: Pubkey,
    entry_pool_sol: Option<f64>,
    status: PosStatus,
    pending_buy: Option<String>,
    pending_sell: Option<PendingSell>,
    queued_sell: u64,
    sell_attempts: u32,
    adds: u32,
    min_mult: f64,
    max_mult: f64,
    leader_exit_mult: Option<f64>,
    exited_ms: Option<i64>,
}

impl Position {
    fn mult(&self) -> f64 {
        self.state.multiple()
    }
}

#[derive(Debug)]
pub enum OrderResult {
    Buy {
        mint: Pubkey,
        outcome: anyhow::Result<SendOutcome>,
        confirmation: Option<Confirmation>,
        reaction_ms: u64,
    },
    Sell {
        mint: Pubkey,
        tokens: u64,
        reason: String,
        outcome: anyhow::Result<SendOutcome>,
        confirmation: Option<Confirmation>,
    },
    Template {
        mint: Pubkey,
        template: Box<anyhow::Result<Template>>,
    },
    Balance(u64),
    Intel {
        mint: Pubkey,
        intel: Option<Box<(TokenIntel, serde_json::Value)>>,
        error: Option<String>,
    },
    TokenBalance {
        mint: Pubkey,
        amount: u64,
    },
}

/// A copy entry waiting for GMGN token intelligence (intel gate).
struct PendingEntry {
    observed_ns: u64,
    swap: DetectedSwap,
    leader: LeaderConfig,
    lamports: u64,
    signal: serde_json::Value,
}

#[derive(Default)]
struct Stats {
    signals: u64,
    copies: u64,
    skips: u64,
    sends: u64,
    landed: u64,
    failed: u64,
    reaction_ms: VecDeque<u64>,
    detect_lag_slots: VecDeque<u64>,
    skip_reasons: HashMap<String, u64>,
}

fn pctl(v: &VecDeque<u64>, p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut s: Vec<u64> = v.iter().copied().collect();
    s.sort_unstable();
    s[((s.len() - 1) as f64 * p).round() as usize]
}

fn push_bounded(v: &mut VecDeque<u64>, x: u64) {
    if v.len() >= 1000 {
        v.pop_front();
    }
    v.push_back(x);
}

pub struct Engine {
    cfg: BotConfig,
    mode: RunMode,
    me: Pubkey,
    exec: Option<Arc<Exec>>,
    gmgn: Option<Arc<Gmgn>>,
    pending: HashMap<Pubkey, PendingEntry>,
    journal: Journal,
    leaders: HashMap<Pubkey, LeaderConfig>,
    tokens: HashMap<Pubkey, TokenInfo>,
    positions: HashMap<Pubkey, Position>,
    afterlife: HashMap<Pubkey, Position>,
    filters: watch::Sender<Filters>,
    results_tx: mpsc::Sender<OrderResult>,
    paused: bool,
    killed: bool,
    feed_stale: bool,
    last_slot: u64,
    last_slot_at: Instant,
    day: String,
    day_realized_sol: f64,
    balance: u64,
    seen: (HashSet<String>, VecDeque<String>),
    /// Token mints and dev wallets we never buy (config + `ctl blacklist`).
    blacklist: HashSet<Pubkey>,
    blacklist_path: std::path::PathBuf,
    /// Mints entered this run (for `filters.one_entry_per_token`).
    traded: HashSet<Pubkey>,
    salt: u64,
    stats: Stats,
    started: Instant,
}

impl Engine {
    pub fn new(
        cfg: BotConfig,
        me: Pubkey,
        exec: Option<Arc<Exec>>,
        gmgn: Option<Arc<Gmgn>>,
        journal: Journal,
        filters: watch::Sender<Filters>,
        results_tx: mpsc::Sender<OrderResult>,
    ) -> Self {
        let mut leaders = HashMap::new();
        for l in cfg.engine.leaders.iter().filter(|l| l.enabled) {
            match l.address.parse::<Pubkey>() {
                Ok(pk) => {
                    leaders.insert(pk, l.clone());
                }
                Err(_) => tracing::warn!(
                    "leader '{}' has an invalid address '{}' — ignored",
                    l.label,
                    l.address
                ),
            }
        }
        let blacklist_path = Path::new(&cfg.infra.storage.journal_dir).join("blacklist.txt");
        let mut blacklist = HashSet::new();
        let f = &cfg.engine.filters;
        let saved = std::fs::read_to_string(&blacklist_path).unwrap_or_default();
        for a in f
            .blacklist_mints
            .iter()
            .chain(&f.blacklist_devs)
            .map(String::as_str)
            .chain(saved.lines().map(str::trim).filter(|l| !l.is_empty()))
        {
            match a.parse::<Pubkey>() {
                Ok(pk) => {
                    blacklist.insert(pk);
                }
                Err(_) => tracing::warn!("blacklist entry '{a}' is not an address — ignored"),
            }
        }
        let virtual_balance = sol_to_lamports(
            cfg.engine.risk.max_total_exposure_sol + cfg.engine.risk.min_sol_reserve,
        );
        let e = Self {
            mode: cfg.engine.mode,
            cfg,
            me,
            exec,
            gmgn,
            pending: HashMap::new(),
            journal,
            leaders,
            tokens: HashMap::new(),
            positions: HashMap::new(),
            afterlife: HashMap::new(),
            filters,
            results_tx,
            paused: false,
            killed: false,
            feed_stale: false,
            last_slot: 0,
            last_slot_at: Instant::now(),
            day: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            day_realized_sol: 0.0,
            balance: virtual_balance,
            seen: (HashSet::new(), VecDeque::new()),
            blacklist,
            blacklist_path,
            traded: HashSet::new(),
            salt: rand::random(),
            stats: Stats::default(),
            started: Instant::now(),
        };
        e.push_filters();
        e
    }

    /// Persist runtime blacklist edits so they survive a restart.
    fn save_blacklist(&self) {
        let mut v: Vec<String> = self.blacklist.iter().map(|k| k.to_string()).collect();
        v.sort();
        if let Err(e) = std::fs::write(&self.blacklist_path, v.join("\n") + "\n") {
            tracing::warn!("saving {}: {e}", self.blacklist_path.display());
        }
    }

    /// Operator-visible event: logged and written to the journal.
    fn alert(&self, msg: impl Into<String>) {
        let m = msg.into();
        tracing::info!("{m}");
        self.journal.record("event", json!({"msg": m}));
    }

    fn next_salt(&mut self) -> u64 {
        self.salt = self
            .salt
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.salt >> 16
    }

    fn push_filters(&self) {
        let mut leaders: Vec<String> = self.leaders.keys().map(|k| k.to_string()).collect();
        leaders.push(self.me.to_string());
        let mints = self
            .positions
            .keys()
            .chain(self.afterlife.keys())
            .map(|k| k.to_string())
            .collect();
        let _ = self.filters.send(Filters { leaders, mints });
    }

    fn label(&self, l: &Pubkey) -> String {
        self.leaders
            .get(l)
            .map(|c| {
                if c.label.is_empty() {
                    short(l)
                } else {
                    c.label.clone()
                }
            })
            .unwrap_or_else(|| short(l))
    }

    // ================================================================ main loop

    pub async fn run(
        mut self,
        mut feed: mpsc::Receiver<FeedEvent>,
        mut results: mpsc::Receiver<OrderResult>,
        mut commands: mpsc::Receiver<Request>,
    ) {
        self.journal.record("wallet", json!({"pubkey": self.me.to_string(), "max_balance_sol": self.cfg.engine.risk.max_total_exposure_sol}));
        self.alert(format!(
            "copybot started · mode {:?} · {} leaders · wallet {}",
            self.mode,
            self.leaders.len(),
            short(&self.me)
        ));
        let mut clock = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                ev = feed.recv() => match ev {
                    Some(FeedEvent::Tx(tx)) => self.on_tx(&tx),
                    Some(FeedEvent::Slot { slot, .. }) => {
                        if slot > self.last_slot { self.last_slot = slot; self.last_slot_at = Instant::now(); }
                    }
                    Some(FeedEvent::Status { source, connected, detail }) => {
                        self.journal.record("feed", json!({"source": source, "connected": connected, "detail": detail}));
                        if !connected { tracing::warn!("{source} disconnected: {detail}"); } else { tracing::info!("{source} connected"); }
                    }
                    None => { self.alert("feed closed — engine stopping"); return; }
                },
                r = results.recv() => if let Some(r) = r { self.on_result(r) },
                c = commands.recv() => if let Some((c, reply)) = c { let r = self.on_command(c); let _ = reply.send(r); },
                _ = clock.tick() => self.on_clock(),
            }
        }
    }

    // ================================================================ transactions

    fn on_tx(&mut self, tx: &ChainTx) {
        if tx.slot > self.last_slot {
            self.last_slot = tx.slot;
            self.last_slot_at = Instant::now();
        }
        if tx.source == FeedSource::Shred {
            self.on_pre_exec(tx);
            return;
        }
        for ev in detect::lifecycle(tx) {
            match ev {
                PumpEvent::Create(c) => {
                    let t = self.tokens.entry(c.mint).or_insert_with(|| TokenInfo {
                        template: Template::Generic,
                        price: 0.0,
                        pool_sol: None,
                        creator: None,
                        token_program: c.token_program.unwrap_or(chain::consts::TOKEN_PROGRAM),
                        decimals: 6,
                        created_ms: None,
                        migrated: false,
                    });
                    t.created_ms = Some(c.timestamp * 1000);
                    t.creator = Some(c.creator);
                }
                PumpEvent::Complete(c) => self.on_migrated(c.mint),
                PumpEvent::Trade(_) => {}
            }
        }
        let swaps = detect::all_swaps(tx);
        for s in &swaps {
            self.update_token(s);
            let is_leader = self.leaders.contains_key(&s.wallet);
            self.journal
                .market_swap(crate::journal::swap_row(tx, s, is_leader));
            if s.wallet == self.me {
                continue; // handled below with true balance deltas
            }
            if is_leader {
                match s.side {
                    Side::Buy => self.on_leader_buy(tx, s),
                    Side::Sell => self.on_leader_sell(s),
                }
            }
            self.on_market(s, tx);
        }
        if tx.signers().contains(&self.me) {
            self.on_own_tx(tx);
        }
    }

    fn update_token(&mut self, s: &DetectedSwap) {
        let t = self.tokens.entry(s.mint).or_insert_with(|| TokenInfo {
            template: Template::Generic,
            price: 0.0,
            pool_sol: None,
            creator: None,
            token_program: s.token_program,
            decimals: s.token_decimals,
            created_ms: None,
            migrated: false,
        });
        // Prefer exact templates; once migrated, ignore stale curve templates.
        let keep = match (&t.template, &s.template) {
            (_, Template::Generic) => !matches!(t.template, Template::Generic) && !s.exact,
            (_, Template::Curve { .. }) if t.migrated => true,
            _ => false,
        };
        if !keep {
            t.template = s.template.clone();
        }
        if s.exact || t.price == 0.0 {
            t.price = s.price_sol;
        }
        if s.pool_sol.is_some() {
            t.pool_sol = s.pool_sol;
        }
        if s.creator.is_some() {
            t.creator = s.creator;
        }
        t.token_program = s.token_program;
        t.decimals = s.token_decimals;
        t.migrated |= s.migrated;
    }

    fn mark_seen(&mut self, key: String) -> bool {
        if self.seen.0.contains(&key) {
            return false;
        }
        self.seen.0.insert(key.clone());
        self.seen.1.push_back(key);
        if self.seen.1.len() > 50_000 {
            if let Some(old) = self.seen.1.pop_front() {
                self.seen.0.remove(&old);
            }
        }
        true
    }

    /// Leader buy seen before execution: act only when we already hold a fresh
    /// curve template for the mint (everything else waits for the confirmed tx).
    fn on_pre_exec(&mut self, tx: &ChainTx) {
        let signers: Vec<Pubkey> = tx
            .signers()
            .iter()
            .filter(|s| self.leaders.contains_key(s))
            .copied()
            .collect();
        for leader in signers {
            for b in detect::pre_exec_pump_buys(tx, &leader) {
                let Some(t) = self.tokens.get(&b.mint) else {
                    continue;
                };
                if !matches!(t.template, Template::Curve { .. }) {
                    continue;
                }
                let s = DetectedSwap {
                    wallet: leader,
                    mint: b.mint,
                    side: Side::Buy,
                    venue: Venue::PumpFunCurve,
                    sol_amount: b.sol_amount,
                    token_amount: 0,
                    token_decimals: t.decimals,
                    token_program: t.token_program,
                    price_sol: t.price,
                    pool_sol: t.pool_sol,
                    fraction_sold: None,
                    exact: false,
                    template: t.template.clone(),
                    creator: t.creator,
                    migrated: false,
                };
                self.on_leader_buy(tx, &s);
            }
        }
    }

    // ================================================================ entries

    fn sizing_for(&self, l: &LeaderConfig) -> SizingConfig {
        let mut s = self.cfg.engine.sizing.clone();
        if let Some(p) = l.copy_pct {
            s.copy_pct = p;
        }
        if let Some(m) = l.max_buy_sol {
            s.max_buy_sol = m;
        }
        s
    }

    fn on_leader_buy(&mut self, tx: &ChainTx, s: &DetectedSwap) {
        if !self.mark_seen(format!("{}:{}", tx.signature, s.wallet)) {
            return;
        }
        self.stats.signals += 1;
        let lag = self.last_slot.saturating_sub(tx.slot);
        push_bounded(&mut self.stats.detect_lag_slots, lag);
        let lcfg = self.leaders[&s.wallet].clone();
        let tinfo = self.tokens.get(&s.mint).cloned();
        let entry = EntryContext {
            venue: s.venue,
            leader_buy: s.sol_amount,
            leader_price: s.price_sol,
            current_price: entry_price(&tinfo, s),
            pool_sol: s.pool_sol,
            token_age_secs: tinfo
                .as_ref()
                .and_then(|t| t.created_ms)
                .map(|c| ((now_ms() - c).max(0) / 1000) as u64),
            detection_slot_lag: lag,
            leader_sol_before: tx
                .keys
                .iter()
                .position(|k| *k == s.wallet)
                .and_then(|i| tx.pre_balances.get(i).copied()),
            market_cap_sol: market_cap_sol(s.venue, entry_price(&tinfo, s)),
        };
        let existing = self.positions.get(&s.mint);
        let creator = s.creator.or(tinfo.as_ref().and_then(|t| t.creator));
        let blacklisted = self.blacklist.contains(&s.mint)
            || creator.is_some_and(|c| self.blacklist.contains(&c));
        let already_traded = self.cfg.engine.filters.one_entry_per_token
            && existing.is_none()
            && self.traded.contains(&s.mint);
        let book = BookContext {
            position_cost: existing
                .map(|p| p.cost_lamports + p.pending_cost)
                .unwrap_or(0),
            adds_so_far: existing.map(|p| p.adds).unwrap_or(0),
            open_positions: self.positions.len() as u32,
            open_exposure: self
                .positions
                .values()
                .map(|p| p.cost_lamports + p.pending_cost)
                .sum(),
            free_balance: self.balance,
            kill_switch: self.killed || self.paused || self.feed_stale,
        };
        let sizing = self.sizing_for(&lcfg);
        let routable = !matches!(s.template, Template::Generic) || self.cfg.infra.jupiter.is_some();
        let decision = if blacklisted {
            SizeDecision::Skip {
                reason: SkipReason::Blacklisted,
            }
        } else if already_traded {
            SizeDecision::Skip {
                reason: SkipReason::AlreadyTraded,
            }
        } else if existing.is_some_and(|p| p.leader != s.wallet) {
            SizeDecision::Skip {
                reason: SkipReason::PositionCapReached,
            }
        } else if !routable {
            SizeDecision::Skip {
                reason: SkipReason::VenueNotAllowed,
            }
        } else {
            match sizing::pre_trade_filters(&self.cfg.engine.filters, &entry) {
                Err(reason) => SizeDecision::Skip { reason },
                Ok(()) => sizing::size_buy(&sizing, &self.cfg.engine.risk, &entry, &book),
            }
        };
        let (decision_s, skip, size, capped) = match decision {
            SizeDecision::Buy {
                lamports,
                capped_by,
            } => (
                "buy",
                None,
                Some(lamports),
                capped_by.map(|c| format!("{c:?}")),
            ),
            SizeDecision::Skip { reason } => ("skip", Some(format!("{reason:?}")), None, None),
        };
        let signal = json!({
            "leader": s.wallet.to_string(), "label": lcfg.label, "signature": tx.signature, "slot": tx.slot,
            "tx_index": tx.tx_index, "mint": s.mint.to_string(), "venue": s.venue, "side": "buy",
            "leader_sol": s.sol_amount, "leader_price": s.price_sol, "source": format!("{:?}", tx.source).to_lowercase(),
            "slot_lag": lag, "decision": decision_s, "skip_reason": skip, "size_lamports": size, "capped_by": capped,
            "pool_sol": s.pool_sol, "exact": s.exact, "token_age_secs": entry.token_age_secs,
            "leader_sol_before": entry.leader_sol_before, "market_cap_sol": entry.market_cap_sol,
        });
        let Some(lamports) = size else {
            self.journal.record("signal", signal);
            self.stats.skips += 1;
            *self
                .stats
                .skip_reasons
                .entry(skip.unwrap_or_default())
                .or_default() += 1;
            return;
        };
        let gate = self.cfg.infra.gmgn.as_ref().and_then(|g| g.gate.clone());
        if let (Some(gate), Some(gm)) = (gate, self.gmgn.clone()) {
            if self.pending.contains_key(&s.mint) {
                return; // already waiting on intel for this mint
            }
            self.pending.insert(
                s.mint,
                PendingEntry {
                    observed_ns: tx.observed_at_ns,
                    swap: s.clone(),
                    leader: lcfg.clone(),
                    lamports,
                    signal,
                },
            );
            self.spawn_intel(gm, s.mint, Some(Duration::from_millis(gate.timeout_ms)));
            return;
        }
        self.journal.record("signal", signal);
        self.stats.copies += 1;
        self.enter(tx.observed_at_ns, s, &lcfg, lamports);
        if let (Some(gm), true) = (
            self.gmgn.clone(),
            self.cfg.infra.gmgn.as_ref().is_some_and(|g| g.enrich),
        ) {
            self.spawn_intel(gm, s.mint, None);
        }
    }

    fn spawn_intel(&self, gm: Arc<Gmgn>, mint: Pubkey, timeout: Option<Duration>) {
        let results = self.results_tx.clone();
        tokio::spawn(async move {
            let mint_s = mint.to_string();
            let fut = gm.token_intel(&mint_s);
            let res = match timeout {
                Some(t) => tokio::time::timeout(t, fut)
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("timeout"))),
                None => fut.await,
            };
            let (intel, error) = match res {
                Ok(v) => (Some(Box::new(v)), None),
                Err(e) => (None, Some(e.to_string())),
            };
            let _ = results
                .send(OrderResult::Intel { mint, intel, error })
                .await;
        });
    }

    fn on_intel(
        &mut self,
        mint: Pubkey,
        intel: Option<Box<(TokenIntel, serde_json::Value)>>,
        error: Option<String>,
    ) {
        if let Some(b) = &intel {
            self.journal.record(
                "token_intel",
                json!({"mint": mint.to_string(), "intel": b.0, "raw": b.1}),
            );
        }
        let Some(p) = self.pending.remove(&mint) else {
            return;
        };
        let gate = self
            .cfg
            .infra
            .gmgn
            .as_ref()
            .and_then(|g| g.gate.clone())
            .expect("pending implies gate");
        let verdict = match (&intel, &error) {
            (Some(b), _) => gate.check(&b.0),
            (None, e) if gate.allow_on_timeout => {
                tracing::warn!(
                    "intel unavailable for {mint} ({e:?}); entering per allow_on_timeout"
                );
                Ok(())
            }
            (None, e) => Err(format!(
                "intel unavailable: {}",
                e.clone().unwrap_or_default()
            )),
        };
        // The world moved while we waited: re-check safety and price drift.
        let price_now = self
            .tokens
            .get(&mint)
            .map(|t| t.price)
            .unwrap_or(p.swap.price_sol);
        let drift = if p.swap.price_sol > 0.0 {
            price_now / p.swap.price_sol - 1.0
        } else {
            0.0
        };
        let verdict = verdict.and_then(|_| {
            if self.killed || self.paused || self.feed_stale {
                Err("KillSwitch".to_string())
            } else if drift > self.cfg.engine.filters.max_price_drift_pct {
                Err(format!(
                    "PriceAlreadyMoved {:.1}% while waiting for intel",
                    drift * 100.0
                ))
            } else if self
                .positions
                .get(&mint)
                .is_some_and(|q| q.leader != p.swap.wallet)
            {
                Err("PositionCapReached".to_string())
            } else {
                Ok(())
            }
        });
        let mut signal = p.signal;
        signal["intel_gate"] = json!(verdict.as_ref().err());
        signal["intel_wait_ms"] =
            json!(ChainTx::now_ns().saturating_sub(p.observed_ns) / 1_000_000);
        match verdict {
            Ok(()) => {
                self.journal.record("signal", signal);
                self.stats.copies += 1;
                self.enter(p.observed_ns, &p.swap, &p.leader, p.lamports);
            }
            Err(reason) => {
                signal["decision"] = json!("skip");
                signal["skip_reason"] = json!(format!("IntelGate: {reason}"));
                self.journal.record("signal", signal);
                self.stats.skips += 1;
                *self
                    .stats
                    .skip_reasons
                    .entry("IntelGate".into())
                    .or_default() += 1;
            }
        }
    }

    fn enter(&mut self, observed_ns: u64, s: &DetectedSwap, lcfg: &LeaderConfig, lamports: u64) {
        self.traded.insert(s.mint);
        let template = self
            .tokens
            .get(&s.mint)
            .map(|t| t.template.clone())
            .unwrap_or_else(|| s.template.clone());
        let slip = self.cfg.engine.risk.max_buy_slippage_bps;
        let salt = self.next_salt();
        let fixed_fee = self.fixed_fee_lamports(false);

        // Simulated fill (shadow / paper), or the live plan's expected amount.
        let expected_tokens = match &template {
            Template::Curve { state, fee_bps, .. } => {
                pump::buy_tokens_for_quote(state, lamports, *fee_bps)
            }
            Template::Amm {
                base_reserve,
                quote_reserve,
                fee_bps,
                ..
            } => pump_amm::buy_base_for_quote(*base_reserve, *quote_reserve, lamports, *fee_bps),
            Template::Dbc {
                price_raw, fee_bps, ..
            }
            | Template::LaunchLab {
                price_raw, fee_bps, ..
            } => chain::meteora_dbc::estimate_buy(*price_raw, lamports, *fee_bps),
            Template::Generic => {
                if s.price_sol > 0.0 {
                    ((lamports_to_sol(lamports) / s.price_sol)
                        * 10f64.powi(s.token_decimals as i32)) as u64
                } else {
                    0
                }
            }
        };
        if expected_tokens == 0 {
            self.journal.record(
                "entry_error",
                json!({"mint": s.mint.to_string(), "error": "zero quote"}),
            );
            return;
        }

        let is_add = self.positions.contains_key(&s.mint);
        if self.mode == RunMode::Live {
            let Some(exec) = self.exec.clone() else {
                return;
            };
            let plan = match &template {
                Template::Generic => None,
                t => match exec.buy_ixs(t, lamports, slip, salt) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        self.journal.record(
                            "entry_error",
                            json!({"mint": s.mint.to_string(), "error": e.to_string()}),
                        );
                        return;
                    }
                },
            };
            let mint = s.mint;
            let tip = self.cfg.infra.fees.tip_lamports_buy;
            let cu = self.cfg.infra.fees.cu_limit_buy;
            let results = self.results_tx.clone();
            let observed = observed_ns;
            tokio::spawn(async move {
                let outcome = match plan {
                    Some((ixs, _, _)) => exec.send_ixs(ixs, cu, tip, salt).await,
                    None => exec.jupiter_swap(&SOL_MINT, &mint, lamports, slip).await,
                };
                let reaction_ms = ChainTx::now_ns().saturating_sub(observed) / 1_000_000;
                let confirmation = match &outcome {
                    Ok(o) => Some(exec.resolve(o, Duration::from_secs(20)).await),
                    Err(_) => None,
                };
                let _ = results
                    .send(OrderResult::Buy {
                        mint,
                        outcome,
                        confirmation,
                        reaction_ms,
                    })
                    .await;
            });
            self.stats.sends += 1;
            if is_add {
                if let Some(p) = self.positions.get_mut(&s.mint) {
                    p.adds += 1;
                    p.pending_cost += lamports;
                    p.pending_buy = Some("pending".into());
                }
            } else {
                self.open_position(s, lcfg, lamports, 0, PosStatus::Opening, &template);
            }
        } else {
            // shadow / paper: instant fill at the post-leader quote (= landing right behind the leader)
            if is_add {
                if let Some(p) = self.positions.get_mut(&s.mint) {
                    p.adds += 1;
                    p.cost_lamports += lamports;
                    p.fees_lamports += fixed_fee;
                    p.initial_tokens += expected_tokens;
                    p.held_tokens += expected_tokens;
                    p.state.entry_price = lamports_to_sol(p.cost_lamports)
                        / (p.initial_tokens as f64 / 10f64.powi(p.decimals as i32));
                }
            } else {
                self.open_position(
                    s,
                    lcfg,
                    lamports,
                    expected_tokens,
                    PosStatus::Open,
                    &template,
                );
                if let Some(p) = self.positions.get_mut(&s.mint) {
                    p.fees_lamports += fixed_fee;
                }
            }
            self.journal.record("fill", json!({"mint": s.mint.to_string(), "side": "buy", "mode": self.mode, "sol": lamports, "tokens": expected_tokens, "simulated": true}));
        }
        self.alert(format!(
            "{} BUY {} · {:.3} SOL copying {} ({:.2} SOL) · {:?}",
            if self.mode == RunMode::Live {
                "🟢"
            } else {
                "⚪ [sim]"
            },
            short(&s.mint),
            lamports_to_sol(lamports),
            self.label(&s.wallet),
            lamports_to_sol(s.sol_amount),
            s.venue
        ));
    }

    /// Put tokens already in the wallet (e.g. after a restart) back under exit
    /// management with the default policy. Entry = first price seen afterwards.
    pub fn adopt(&mut self, mint: Pubkey, amount: u64, token_program: Pubkey, decimals: u8) {
        if amount == 0 || self.positions.contains_key(&mint) || mint == chain::consts::WSOL_MINT {
            return;
        }
        let policy = self.cfg.engine.exits[&self.cfg.engine.default_exit].clone();
        let mut state = PositionState::open(&policy, 1.0, now_ms(), None);
        state.entry_price = 0.0;
        let p = Position {
            mint,
            leader: Pubkey::default(),
            leader_label: "recovered".into(),
            opened_ms: now_ms(),
            policy_hash: policy_hash(&policy),
            policy_name: self.cfg.engine.default_exit.clone(),
            policy,
            state,
            path: Vec::new(),
            cost_lamports: 0,
            pending_cost: 0,
            fees_lamports: 0,
            proceeds_lamports: 0,
            initial_tokens: amount,
            held_tokens: amount,
            decimals,
            token_program,
            entry_pool_sol: None,
            status: PosStatus::Open,
            pending_buy: None,
            pending_sell: None,
            queued_sell: 0,
            sell_attempts: 0,
            adds: 0,
            min_mult: 1.0,
            max_mult: 1.0,
            leader_exit_mult: None,
            exited_ms: None,
        };
        self.journal.record(
            "position_adopt",
            json!({"mint": mint.to_string(), "tokens": amount}),
        );
        self.alert(format!(
            "♻️ adopted {} tokens of {} already in the wallet — managed by '{}'",
            amount,
            short(&mint),
            p.policy_name
        ));
        self.positions.insert(mint, p);
        self.push_filters();
    }

    fn open_position(
        &mut self,
        s: &DetectedSwap,
        lcfg: &LeaderConfig,
        cost: u64,
        tokens: u64,
        status: PosStatus,
        _tpl: &Template,
    ) {
        let policy = self.cfg.engine.exit_for(lcfg).clone();
        let policy_name = lcfg
            .exit
            .clone()
            .unwrap_or_else(|| self.cfg.engine.default_exit.clone());
        let price = if tokens > 0 {
            lamports_to_sol(cost) / (tokens as f64 / 10f64.powi(s.token_decimals as i32))
        } else {
            s.price_sol
        };
        let pool = s.pool_sol.map(lamports_to_sol);
        let now = now_ms();
        let state = PositionState::open(&policy, price, now, pool);
        let p = Position {
            mint: s.mint,
            leader: s.wallet,
            leader_label: lcfg.label.clone(),
            opened_ms: now,
            policy_hash: policy_hash(&policy),
            policy_name,
            policy,
            state,
            path: Vec::new(),
            cost_lamports: if status == PosStatus::Open { cost } else { 0 },
            pending_cost: if status == PosStatus::Opening {
                cost
            } else {
                0
            },
            fees_lamports: 0,
            proceeds_lamports: 0,
            initial_tokens: tokens,
            held_tokens: tokens,
            decimals: s.token_decimals,
            token_program: s.token_program,
            entry_pool_sol: pool,
            status,
            pending_buy: (status == PosStatus::Opening).then(|| "pending".into()),
            pending_sell: None,
            queued_sell: 0,
            sell_attempts: 0,
            adds: 0,
            min_mult: 1.0,
            max_mult: 1.0,
            leader_exit_mult: None,
            exited_ms: None,
        };
        self.journal.record("position_open", json!({"mint": s.mint.to_string(), "leader": s.wallet.to_string(), "mode": self.mode, "cost_lamports": cost, "tokens": tokens, "entry_price": price, "exit_policy": p.policy_name}));
        self.positions.insert(s.mint, p);
        self.push_filters();
    }

    // ================================================================ market / exits

    fn on_leader_sell(&mut self, s: &DetectedSwap) {
        let Some(p) = self.positions.get_mut(&s.mint) else {
            return;
        };
        if p.leader != s.wallet {
            return;
        }
        if p.leader_exit_mult.is_none() && s.fraction_sold.unwrap_or(1.0) > 0.9 {
            p.leader_exit_mult = Some(p.mult());
        }
        let ev = MarketEvent::LeaderSell {
            t_ms: now_ms(),
            fraction: s.fraction_sold.unwrap_or(1.0),
        };
        self.journal.record("leader_sell", json!({"mint": s.mint.to_string(), "leader": s.wallet.to_string(), "fraction": s.fraction_sold, "sol": s.sol_amount}));
        self.apply_event(s.mint, ev);
    }

    fn on_market(&mut self, s: &DetectedSwap, _tx: &ChainTx) {
        let creator = self.tokens.get(&s.mint).and_then(|t| t.creator);
        for map in [true, false] {
            let pos = if map {
                self.positions.get_mut(&s.mint)
            } else {
                self.afterlife.get_mut(&s.mint)
            };
            let Some(p) = pos else { continue };
            if s.price_sol > 0.0 && p.path.len() < PATH_CAP {
                p.path.push(MarketEvent::Price {
                    t_ms: now_ms(),
                    price: s.price_sol,
                });
                if let Some(ps) = s.pool_sol {
                    p.path.push(MarketEvent::Liquidity {
                        t_ms: now_ms(),
                        pool_sol: lamports_to_sol(ps),
                    });
                }
            }
        }
        if !self.positions.contains_key(&s.mint) {
            return;
        }
        let t = now_ms();
        if s.price_sol > 0.0 {
            self.apply_event(
                s.mint,
                MarketEvent::Price {
                    t_ms: t,
                    price: s.price_sol,
                },
            );
        }
        if let Some(ps) = s.pool_sol {
            self.apply_event(
                s.mint,
                MarketEvent::Liquidity {
                    t_ms: t,
                    pool_sol: lamports_to_sol(ps),
                },
            );
        }
        if s.side == Side::Sell && creator == Some(s.wallet) {
            self.journal.record(
                "dev_sell",
                json!({"mint": s.mint.to_string(), "sol": s.sol_amount}),
            );
            self.apply_event(s.mint, MarketEvent::DevSell { t_ms: t });
        }
    }

    /// Feed one event to the live policy; execute resulting sells.
    fn apply_event(&mut self, mint: Pubkey, ev: MarketEvent) {
        let Some(p) = self.positions.get_mut(&mint) else {
            return;
        };
        if !matches!(
            ev,
            MarketEvent::Price { .. } | MarketEvent::Liquidity { .. } | MarketEvent::Clock { .. }
        ) && p.path.len() < PATH_CAP
        {
            p.path.push(ev);
        }
        if p.status != PosStatus::Open || p.held_tokens == 0 {
            return;
        }
        // Adopted (recovered) positions get their entry price from the first trade seen.
        if p.state.entry_price <= 0.0 {
            let MarketEvent::Price { t_ms, price } = ev else {
                return;
            };
            p.state = PositionState::open(&p.policy, price, t_ms, None);
            return;
        }
        let actions = p.policy.on_event(&mut p.state, &ev);
        let m = p.mult();
        p.min_mult = p.min_mult.min(m);
        p.max_mult = p.max_mult.max(m);
        for a in actions {
            self.sell_for_action(mint, a);
        }
    }

    fn sell_for_action(&mut self, mint: Pubkey, a: ExitAction) {
        let Some(p) = self.positions.get(&mint) else {
            return;
        };
        let all = p.state.is_closed();
        let tokens = if all {
            p.held_tokens
        } else {
            ((p.initial_tokens as f64) * a.sell_fraction).round() as u64
        };
        let urgent = matches!(
            a.reason,
            ExitReason::StopLoss | ExitReason::DevSell | ExitReason::LiquidityDrop
        );
        self.sell(mint, tokens, &format!("{:?}", a.reason), urgent);
    }

    fn fixed_fee_lamports(&self, sell: bool) -> u64 {
        let f = &self.cfg.infra.fees;
        let cu = if sell {
            f.cu_limit_sell
        } else {
            f.cu_limit_buy
        } as u64;
        let tip = if sell {
            f.tip_lamports_sell
        } else {
            f.tip_lamports_buy
        };
        5000 + cu * f.cu_price_micro_lamports / 1_000_000 + tip
    }

    fn sell(&mut self, mint: Pubkey, tokens: u64, reason: &str, urgent: bool) {
        let slip = self.cfg.engine.risk.max_sell_slippage_bps;
        let salt = self.next_salt();
        let fixed_fee = self.fixed_fee_lamports(true);
        let template = self
            .tokens
            .get(&mint)
            .map(|t| t.template.clone())
            .unwrap_or(Template::Generic);
        let price_now = self.tokens.get(&mint).map(|t| t.price).unwrap_or(0.0);
        let mode = self.mode;
        let Some(p) = self.positions.get_mut(&mint) else {
            return;
        };
        let tokens = tokens.min(p.held_tokens);
        if tokens == 0 {
            return;
        }
        if mode != RunMode::Live {
            let out = match &template {
                Template::Curve { state, fee_bps, .. } => {
                    pump::sell_quote_for_tokens(state, tokens, *fee_bps)
                }
                Template::Amm {
                    base_reserve,
                    quote_reserve,
                    fee_bps,
                    ..
                } => pump_amm::sell_quote_for_base(*base_reserve, *quote_reserve, tokens, *fee_bps),
                Template::Dbc {
                    price_raw, fee_bps, ..
                }
                | Template::LaunchLab {
                    price_raw, fee_bps, ..
                } => chain::meteora_dbc::estimate_sell(*price_raw, tokens, *fee_bps),
                Template::Generic => {
                    sol_to_lamports(price_now * tokens as f64 / 10f64.powi(p.decimals as i32))
                }
            };
            p.held_tokens -= tokens;
            p.proceeds_lamports += out;
            p.fees_lamports += fixed_fee;
            self.journal.record("fill", json!({"mint": mint.to_string(), "side": "sell", "mode": mode, "sol": out, "tokens": tokens, "reason": reason, "simulated": true}));
            if p.held_tokens == 0 {
                self.on_exited(mint);
            }
            return;
        }
        if p.pending_sell.is_some() {
            p.queued_sell = (p.queued_sell + tokens).min(p.held_tokens);
            return;
        }
        let close = tokens >= p.held_tokens;
        let token_program = p.token_program;
        p.pending_sell = Some(PendingSell {
            signature: String::new(),
        });
        p.sell_attempts += 1;
        let attempt = p.sell_attempts;
        let Some(exec) = self.exec.clone() else {
            return;
        };
        let f = &self.cfg.infra.fees;
        let mut tip = f.tip_lamports_sell;
        if urgent {
            tip *= f.urgent_tip_multiplier;
        }
        tip = tip.saturating_mul(1 << (attempt.saturating_sub(1).min(3))); // escalate on retries
        let cu = f.cu_limit_sell;
        let reason = reason.to_string();
        let results = self.results_tx.clone();
        self.stats.sends += 1;
        tokio::spawn(async move {
            let outcome = match &template {
                Template::Generic => exec.jupiter_swap(&mint, &SOL_MINT, tokens, slip).await,
                t => match exec.sell_ixs(t, &token_program, &mint, tokens, slip, close, salt) {
                    Ok((ixs, _)) => exec.send_ixs(ixs, cu, tip, salt).await,
                    Err(e) => Err(e),
                },
            };
            let confirmation = match &outcome {
                Ok(o) => Some(exec.resolve(o, Duration::from_secs(20)).await),
                Err(_) => None,
            };
            let _ = results
                .send(OrderResult::Sell {
                    mint,
                    tokens,
                    reason,
                    outcome,
                    confirmation,
                })
                .await;
        });
    }

    fn on_migrated(&mut self, mint: Pubkey) {
        if let Some(t) = self.tokens.get_mut(&mint) {
            t.migrated = true;
        }
        if !self.positions.contains_key(&mint) {
            return;
        }
        self.journal
            .record("migrated", json!({"mint": mint.to_string()}));
        self.alert(format!(
            "🎓 {} graduated to PumpSwap — switching exit route",
            short(&mint)
        ));
        if let (Some(exec), Some(t)) = (self.exec.clone(), self.tokens.get(&mint)) {
            let tp = t.token_program;
            let salt = self.next_salt();
            let results = self.results_tx.clone();
            tokio::spawn(async move {
                // the pool is created in the same tx; give RPC a moment to see it
                tokio::time::sleep(Duration::from_millis(800)).await;
                let template = exec.migrated_template(&mint, &tp, salt).await;
                let _ = results
                    .send(OrderResult::Template {
                        mint,
                        template: Box::new(template),
                    })
                    .await;
            });
        }
    }

    // ================================================================ our own fills (live)

    fn on_own_tx(&mut self, tx: &ChainTx) {
        for s in detect::balance_swaps(tx, &self.me) {
            let Some(p) = self.positions.get_mut(&s.mint) else {
                continue;
            };
            match s.side {
                Side::Buy => {
                    p.cost_lamports += s.sol_amount;
                    p.pending_cost = 0;
                    p.initial_tokens += s.token_amount;
                    p.held_tokens += s.token_amount;
                    p.pending_buy = None;
                    if p.initial_tokens > 0 {
                        p.state.entry_price = lamports_to_sol(p.cost_lamports)
                            / (p.initial_tokens as f64 / 10f64.powi(p.decimals as i32));
                        p.state.last_price = p.state.entry_price;
                        p.state.peak_price = p.state.peak_price.max(p.state.entry_price);
                    }
                    p.status = PosStatus::Open;
                    self.journal.record("fill", json!({"mint": s.mint.to_string(), "side": "buy", "signature": tx.signature, "slot": tx.slot, "sol": s.sol_amount, "tokens": s.token_amount, "simulated": false}));
                }
                Side::Sell => {
                    p.held_tokens = p.held_tokens.saturating_sub(s.token_amount);
                    p.proceeds_lamports += s.sol_amount;
                    self.journal.record("fill", json!({"mint": s.mint.to_string(), "side": "sell", "signature": tx.signature, "slot": tx.slot, "sol": s.sol_amount, "tokens": s.token_amount, "simulated": false}));
                    if p.held_tokens == 0 {
                        let mint = s.mint;
                        self.on_exited(mint);
                    }
                }
            }
        }
    }

    fn on_result(&mut self, r: OrderResult) {
        match r {
            OrderResult::Buy {
                mint,
                outcome,
                confirmation,
                reaction_ms,
            } => {
                push_bounded(&mut self.stats.reaction_ms, reaction_ms);
                let (sig, reports, err) = match &outcome {
                    Ok(o) => (
                        o.signature.clone(),
                        serde_json::to_value(&o.reports).unwrap_or_default(),
                        None,
                    ),
                    Err(e) => (String::new(), json!([]), Some(e.to_string())),
                };
                let status = match &confirmation {
                    Some(Confirmation::Landed { .. }) => "landed",
                    Some(Confirmation::Failed { .. }) => "failed",
                    Some(Confirmation::Expired) => "expired",
                    None => "failed",
                };
                let landed_slot = match &confirmation {
                    Some(Confirmation::Landed { slot, .. }) => Some(*slot),
                    _ => None,
                };
                self.journal.record("order", json!({"side": "buy", "reason": "copy", "mint": mint.to_string(), "signature": sig, "senders": reports, "status": status, "error": err.or_else(|| match &confirmation { Some(Confirmation::Failed { err, .. }) => Some(err.clone()), _ => None }), "reaction_ms": reaction_ms, "landed_slot": landed_slot, "tip_lamports": self.cfg.infra.fees.tip_lamports_buy, "cu_limit": self.cfg.infra.fees.cu_limit_buy}));
                if status == "landed" {
                    self.stats.landed += 1;
                    // Fill normally arrives via the feed; fetch the balance as a fallback.
                    if let Some(exec) = self.exec.clone() {
                        let results = self.results_tx.clone();
                        let me = self.me;
                        let tp = self
                            .positions
                            .get(&mint)
                            .map(|p| p.token_program)
                            .unwrap_or(chain::consts::TOKEN_PROGRAM);
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            if let Ok(accs) = exec.rpc.token_accounts(&me, &tp).await {
                                let amount = accs
                                    .iter()
                                    .filter(|(_, m, _)| *m == mint)
                                    .map(|(_, _, a)| *a)
                                    .sum();
                                let _ = results
                                    .send(OrderResult::TokenBalance { mint, amount })
                                    .await;
                            }
                        });
                    }
                } else {
                    self.stats.failed += 1;
                    if let Some(p) = self.positions.get_mut(&mint) {
                        p.pending_buy = None;
                        p.pending_cost = 0;
                        if p.initial_tokens == 0 {
                            self.positions.remove(&mint);
                            self.push_filters();
                        }
                    }
                    self.alert(format!("⚠️ buy {} not landed ({status})", short(&mint)));
                }
            }
            OrderResult::Sell {
                mint,
                tokens,
                reason,
                outcome,
                confirmation,
            } => {
                let landed = matches!(confirmation, Some(Confirmation::Landed { .. }));
                let sig = outcome
                    .as_ref()
                    .map(|o| o.signature.clone())
                    .unwrap_or_default();
                self.journal.record("order", json!({"side": "sell", "reason": reason, "mint": mint.to_string(), "signature": sig, "tokens": tokens, "status": if landed { "landed" } else { "failed" }, "error": outcome.as_ref().err().map(|e| e.to_string()), "confirmation": format!("{confirmation:?}")}));
                if landed {
                    self.stats.landed += 1;
                } else {
                    self.stats.failed += 1;
                }
                let mut retry: Option<(u64, bool)> = None;
                if let Some(p) = self.positions.get_mut(&mint) {
                    if let Some(ps) = &mut p.pending_sell {
                        ps.signature = sig;
                    }
                    p.pending_sell = None;
                    if landed {
                        p.sell_attempts = 0;
                        if p.queued_sell > 0 {
                            retry = Some((std::mem::take(&mut p.queued_sell), false));
                        }
                    } else if p.sell_attempts < 5 {
                        retry = Some((tokens + std::mem::take(&mut p.queued_sell), true));
                    } else {
                        let m = format!(
                            "🛑 sell of {} failed {} times — manual action needed",
                            short(&mint),
                            p.sell_attempts
                        );
                        self.journal.record(
                            "risk",
                            json!({"risk": "stuck_position", "mint": mint.to_string()}),
                        );
                        self.alert(m);
                    }
                }
                if let Some((t, urgent)) = retry {
                    self.sell(mint, t, &format!("{reason}+retry"), urgent);
                }
            }
            OrderResult::Template { mint, template } => match *template {
                Ok(t) => {
                    if let Some(ti) = self.tokens.get_mut(&mint) {
                        ti.template = t;
                    }
                }
                Err(e) => tracing::warn!("migrated template for {mint}: {e}"),
            },
            OrderResult::Balance(b) => self.balance = b,
            OrderResult::Intel { mint, intel, error } => self.on_intel(mint, intel, error),
            OrderResult::TokenBalance { mint, amount } => {
                if let Some(p) = self.positions.get_mut(&mint) {
                    if p.status == PosStatus::Opening && amount > 0 {
                        p.cost_lamports = p.pending_cost;
                        p.pending_cost = 0;
                        p.initial_tokens = amount;
                        p.held_tokens = amount;
                        p.status = PosStatus::Open;
                        tracing::warn!("fill for {mint} taken from balance (feed missed it)");
                    }
                }
            }
        }
    }

    // ================================================================ closing

    fn on_exited(&mut self, mint: Pubkey) {
        let Some(mut p) = self.positions.remove(&mint) else {
            return;
        };
        let pnl = lamports_to_sol(p.proceeds_lamports)
            - lamports_to_sol(p.cost_lamports)
            - lamports_to_sol(p.fees_lamports);
        self.day_realized_sol += pnl;
        p.exited_ms = Some(now_ms());
        let ret = if p.cost_lamports > 0 {
            pnl / lamports_to_sol(p.cost_lamports)
        } else {
            0.0
        };
        self.journal.record(
            "position_exit",
            json!({"mint": mint.to_string(), "leader": p.leader.to_string(), "pnl_sol": pnl, "ret": ret, "held_ms": now_ms() - p.opened_ms, "max_mult": p.max_mult, "min_mult": p.min_mult, "leader_exit_mult": p.leader_exit_mult}),
        );
        self.alert(format!(
            "{} EXIT {} · {:+.4} SOL ({:+.1}%) · peak {:.2}x · leader {}",
            if pnl >= 0.0 { "✅" } else { "🔻" },
            short(&mint),
            pnl,
            ret * 100.0,
            p.max_mult,
            p.leader_label
        ));
        self.afterlife.insert(mint, p);
        self.check_daily_loss();
        self.push_filters();
    }

    /// After the afterlife window, score every shadow policy on the full path and write the final record.
    fn finalize(&mut self, mint: Pubkey) {
        let Some(p) = self.afterlife.remove(&mint) else {
            return;
        };
        let size = lamports_to_sol(p.cost_lamports);
        let fee_bps = match self.tokens.get(&mint).map(|t| &t.template) {
            Some(Template::Curve { fee_bps, .. })
            | Some(Template::Amm { fee_bps, .. })
            | Some(Template::Dbc { fee_bps, .. })
            | Some(Template::LaunchLab { fee_bps, .. }) => *fee_bps as f64,
            _ => 100.0,
        };
        let cost = CostModel {
            fee_bps,
            slippage_bps: 50.0,
            fixed_sol_per_tx: lamports_to_sol(self.fixed_fee_lamports(true)),
        };
        let mut names = vec![p.policy_name.clone()];
        names.extend(self.cfg.engine.shadow_exits.iter().cloned());
        names.dedup();
        let mut shadows = vec![];
        for n in names {
            let Some(pol) = self.cfg.engine.exits.get(&n) else {
                continue;
            };
            let input = PathInput {
                entry_price: p.state.entry_price,
                entry_t_ms: p.opened_ms,
                entry_pool_sol: p.entry_pool_sol,
                size_sol: size,
                events: &p.path,
            };
            let r = replay::replay(pol, &input, &cost);
            shadows.push(json!({"policy": n, "policy_hash": policy_hash(pol), "pnl_sol": r.pnl_sol, "ret": r.ret, "held_ms": r.held_ms, "fully_closed": r.fully_closed, "exits": r.exits}));
        }
        let pnl = lamports_to_sol(p.proceeds_lamports)
            - lamports_to_sol(p.cost_lamports)
            - lamports_to_sol(p.fees_lamports);
        self.journal.record(
            "position_close",
            json!({
                "mint": mint.to_string(), "leader": p.leader.to_string(), "wallet": self.me.to_string(), "mode": self.mode,
                "exit_policy": p.policy_name, "exit_policy_hash": p.policy_hash, "opened_at": p.opened_ms, "exited_at": p.exited_ms,
                "entry_price": p.state.entry_price, "entry_pool_sol": p.entry_pool_sol, "cost_lamports": p.cost_lamports,
                "proceeds_lamports": p.proceeds_lamports, "fees_lamports": p.fees_lamports, "peak_multiple": p.max_mult,
                "trough_multiple": p.min_mult, "realized_pnl_sol": pnl, "leader_exit_multiple": p.leader_exit_mult,
                "path_events": p.path.len(), "shadows": shadows,
            }),
        );
        self.push_filters();
    }

    fn check_daily_loss(&mut self) {
        if !self.killed && -self.day_realized_sol >= self.cfg.engine.risk.daily_loss_limit_sol {
            self.killed = true;
            self.journal.record(
                "risk",
                json!({"risk": "daily_loss", "realized_sol": self.day_realized_sol}),
            );
            self.alert(format!(
                "🛑 KILL SWITCH: daily loss {:.3} SOL ≥ limit — no new entries today",
                -self.day_realized_sol
            ));
        }
    }

    // ================================================================ clock / commands

    fn on_clock(&mut self) {
        let t = now_ms();
        let mints: Vec<Pubkey> = self.positions.keys().copied().collect();
        for m in mints {
            self.apply_event(m, MarketEvent::Clock { t_ms: t });
        }
        let done: Vec<Pubkey> = self
            .afterlife
            .iter()
            .filter(|(_, p)| p.exited_ms.is_some_and(|e| t - e > AFTERLIFE_MS))
            .map(|(m, _)| *m)
            .collect();
        for m in done {
            self.finalize(m);
        }
        let stale =
            self.last_slot_at.elapsed() > Duration::from_secs(self.cfg.infra.feed_stale_secs);
        if stale != self.feed_stale {
            self.feed_stale = stale;
            self.journal.record(
                "risk",
                json!({"risk": if stale { "feed_stale" } else { "feed_recovered" }}),
            );
            self.alert(if stale {
                "⚠️ feed stale — entries paused"
            } else {
                "feed recovered — entries resumed"
            });
        }
        let d = chrono::Utc::now().format("%Y-%m-%d").to_string();
        if d != self.day {
            self.day = d;
            self.day_realized_sol = 0.0;
            if self.killed {
                self.killed = false;
                self.alert("new UTC day — daily kill switch reset");
            }
        }
        if self.mode == RunMode::Live && self.started.elapsed().as_secs().is_multiple_of(10) {
            if let Some(exec) = self.exec.clone() {
                let results = self.results_tx.clone();
                tokio::spawn(async move {
                    if let Ok(b) = exec.rpc.balance(&exec.me()).await {
                        let _ = results.send(OrderResult::Balance(b)).await;
                    }
                });
            }
        }
    }

    fn on_command(&mut self, c: Command) -> String {
        let msg = match c {
            Command::Pause => {
                self.paused = true;
                "entries paused (exits keep running)".to_string()
            }
            Command::Resume => {
                self.paused = false;
                "entries resumed".to_string()
            }
            Command::Kill => {
                self.killed = true;
                self.journal.record("risk", json!({"risk": "manual_kill"}));
                "kill switch ON: no new entries".to_string()
            }
            Command::Flatten => {
                self.paused = true;
                let mints: Vec<(Pubkey, u64)> = self
                    .positions
                    .iter()
                    .map(|(m, p)| (*m, p.held_tokens))
                    .collect();
                for (m, t) in &mints {
                    if let Some(p) = self.positions.get_mut(m) {
                        p.state.remaining = 0.0;
                    }
                    self.sell(*m, *t, "Flatten", true);
                }
                format!("flattening {} positions; entries paused", mints.len())
            }
            Command::Blacklist(None) => {
                let mut v: Vec<String> = self.blacklist.iter().map(|k| k.to_string()).collect();
                v.sort();
                if v.is_empty() {
                    "blacklist is empty".to_string()
                } else {
                    format!("blacklist ({}):\n{}", v.len(), v.join("\n"))
                }
            }
            Command::Blacklist(Some(pk)) => {
                self.blacklist.insert(pk);
                self.save_blacklist();
                self.journal
                    .record("blacklist", json!({"add": pk.to_string()}));
                format!(
                    "blacklisted {pk}: no buys of this mint or of tokens created by this wallet"
                )
            }
            Command::Unblacklist(pk) => {
                let was = self.blacklist.remove(&pk);
                self.save_blacklist();
                self.journal
                    .record("blacklist", json!({"remove": pk.to_string()}));
                if was {
                    format!("removed {pk} (entries in the config file stay until edited there)")
                } else {
                    format!("{pk} was not blacklisted")
                }
            }
            Command::Leaders => {
                let s: Vec<String> = self
                    .leaders
                    .iter()
                    .map(|(k, l)| {
                        format!(
                            "{} {}",
                            if l.label.is_empty() {
                                short(k)
                            } else {
                                l.label.clone()
                            },
                            short(k)
                        )
                    })
                    .collect();
                format!("leaders:\n{}", s.join("\n"))
            }
            Command::Positions => {
                if self.positions.is_empty() {
                    "no open positions".to_string()
                } else {
                    let s: Vec<String> = self
                        .positions
                        .values()
                        .map(|p| {
                            format!(
                                "{} {:.2}x (peak {:.2}x) cost {:.3} SOL · {} · {}",
                                short(&p.mint),
                                p.mult(),
                                p.max_mult,
                                lamports_to_sol(p.cost_lamports),
                                p.leader_label,
                                p.policy_name
                            )
                        })
                        .collect();
                    s.join("\n")
                }
            }
            Command::Status => {
                let s = &self.stats;
                format!(
                    "mode {:?} · up {}m · {}\nsignals {} · copies {} · skips {} · sends {} · landed {} · failed {}\nreaction p50 {} ms / p90 {} ms · detect lag p50 {} / p90 {} slots\nopen {} · exposure {:.3} SOL · today {:+.4} SOL · balance {:.3} SOL\ntop skips: {}",
                    self.mode,
                    self.started.elapsed().as_secs() / 60,
                    if self.killed { "KILLED" } else if self.paused { "PAUSED" } else if self.feed_stale { "FEED STALE" } else { "running" },
                    s.signals, s.copies, s.skips, s.sends, s.landed, s.failed,
                    pctl(&s.reaction_ms, 0.5), pctl(&s.reaction_ms, 0.9),
                    pctl(&s.detect_lag_slots, 0.5), pctl(&s.detect_lag_slots, 0.9),
                    self.positions.len(),
                    lamports_to_sol(self.positions.values().map(|p| p.cost_lamports).sum()),
                    self.day_realized_sol,
                    lamports_to_sol(self.balance),
                    top_skips(&s.skip_reasons),
                )
            }
        };
        self.journal.record(
            "control",
            json!({"command": format!("{c:?}"), "reply": msg}),
        );
        msg
    }
}

fn top_skips(m: &HashMap<String, u64>) -> String {
    let mut v: Vec<(&String, &u64)> = m.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1));
    v.iter()
        .take(4)
        .map(|(k, n)| format!("{k} {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn short(p: &Pubkey) -> String {
    let s = p.to_string();
    format!("{}…{}", &s[..4], &s[s.len() - 4..])
}

pub fn policy_hash(p: &ExitPolicy) -> String {
    use sha2::{Digest, Sha256};
    let s = serde_json::to_string(p).unwrap_or_default();
    let h = Sha256::digest(s.as_bytes());
    h[..6].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chain::borsh::EVENT_IX_TAG;
    use chain::consts::{PUMP_PROGRAM, TOKEN_PROGRAM};
    use chain::model::{Ix, TokenBal};

    struct Ev {
        mint: Pubkey,
        user: Pubkey,
        creator: Pubkey,
        is_buy: bool,
        sol: u64,
        tokens: u64,
        vsol: u64,
        vtok: u64,
        real_sol: u64,
    }

    fn trade_tx(slot: u64, e: Ev, pre_user_tokens: u64) -> ChainTx {
        let mut d = EVENT_IX_TAG.to_vec();
        d.extend(pump::event_disc("TradeEvent"));
        d.extend(e.mint.to_bytes());
        d.extend(e.sol.to_le_bytes());
        d.extend(e.tokens.to_le_bytes());
        d.push(e.is_buy as u8);
        d.extend(e.user.to_bytes());
        d.extend(1_700_000_000i64.to_le_bytes());
        for v in [e.vsol, e.vtok, e.real_sol, 700_000_000_000_000u64] {
            d.extend(v.to_le_bytes());
        }
        d.extend(chain::consts::PUMP_FEE_RECIPIENTS[0].to_bytes());
        d.extend(95u64.to_le_bytes());
        d.extend(0u64.to_le_bytes());
        d.extend(e.creator.to_bytes());
        d.extend(30u64.to_le_bytes());
        d.extend(0u64.to_le_bytes());
        ChainTx {
            signature: format!("sig{slot}"),
            slot,
            tx_index: Some(1),
            block_time_ms: None,
            observed_at_ns: ChainTx::now_ns(),
            source: FeedSource::Geyser,
            failed: false,
            fee: 5000,
            keys: vec![e.user],
            num_signers: 1,
            top: vec![Ix {
                program: PUMP_PROGRAM,
                accounts: vec![],
                data: vec![],
            }],
            inner: vec![(
                0,
                vec![Ix {
                    program: PUMP_PROGRAM,
                    accounts: vec![],
                    data: d,
                }],
            )],
            pre_balances: vec![],
            post_balances: vec![],
            pre_tokens: vec![TokenBal {
                account: Pubkey::new_unique(),
                mint: e.mint,
                owner: e.user,
                program: TOKEN_PROGRAM,
                amount: pre_user_tokens,
                decimals: 6,
            }],
            post_tokens: vec![],
            logs: vec![],
            has_meta: true,
        }
    }

    const VTOK: u64 = 1_000_000_000_000_000;

    fn engine(dir: &str, leader: Pubkey, exit: &str) -> (Engine, mpsc::Receiver<OrderResult>) {
        let s = include_str!("../../../../config/copybot.example.toml");
        let mut cfg = BotConfig::from_toml(s).unwrap();
        cfg.engine.mode = RunMode::Paper;
        cfg.engine.leaders = vec![LeaderConfig {
            address: leader.to_string(),
            label: "L".into(),
            enabled: true,
            copy_pct: None,
            max_buy_sol: None,
            exit: Some(exit.into()),
        }];
        cfg.infra.storage.journal_dir = dir.into();
        let journal = futures::executor::block_on(Journal::start(dir, None, None)).unwrap();
        let (ftx, _frx) = watch::channel(Filters::default());
        let (rtx, rrx) = mpsc::channel(100);
        (
            Engine::new(cfg, Pubkey::new_unique(), None, None, journal, ftx, rtx),
            rrx,
        )
    }

    fn records(dir: &str) -> Vec<serde_json::Value> {
        let mut out = vec![];
        for f in std::fs::read_dir(dir).unwrap() {
            let path = f.unwrap().path();
            if path.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            for l in std::fs::read_to_string(path).unwrap().lines() {
                out.push(serde_json::from_str(l).unwrap());
            }
        }
        out
    }

    #[tokio::test]
    async fn copies_takes_profit_and_trails_out() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        let (leader, mint, creator, other) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let (mut e, _r) = engine(d, leader, "ladder_trail");

        // leader buys 1 SOL; curve at 30 SOL virtual, 30 SOL real liquidity
        e.on_tx(&trade_tx(
            100,
            Ev {
                mint,
                user: leader,
                creator,
                is_buy: true,
                sol: 1_000_000_000,
                tokens: 1,
                vsol: 30_000_000_000,
                vtok: VTOK,
                real_sol: 30_000_000_000,
            },
            0,
        ));
        let p = e.positions.get(&mint).expect("position opened");
        assert_eq!(p.cost_lamports, 100_000_000, "10% of the leader's 1 SOL");
        assert!(p.held_tokens > 0);
        let initial = p.initial_tokens;

        // price doubles (someone else buys) → first take-profit (40%) fires
        e.on_tx(&trade_tx(
            101,
            Ev {
                mint,
                user: other,
                creator,
                is_buy: true,
                sol: 5,
                tokens: 1,
                vsol: 61_000_000_000,
                vtok: VTOK,
                real_sol: 61_000_000_000,
            },
            0,
        ));
        let p = e.positions.get(&mint).unwrap();
        let sold = initial - p.held_tokens;
        assert!(
            (sold as f64 / initial as f64 - 0.4).abs() < 0.001,
            "sold {sold} of {initial}"
        );

        // falls >30% from the 2x peak → trailing stop exits the rest
        e.on_tx(&trade_tx(
            102,
            Ev {
                mint,
                user: other,
                creator,
                is_buy: false,
                sol: 5,
                tokens: 1,
                vsol: 40_000_000_000,
                vtok: VTOK,
                real_sol: 40_000_000_000,
            },
            0,
        ));
        assert!(!e.positions.contains_key(&mint), "fully exited");
        assert!(e.afterlife.contains_key(&mint), "kept for shadow scoring");
        e.finalize(mint);

        tokio::time::sleep(Duration::from_millis(200)).await;
        let recs = records(d);
        let kinds: Vec<&str> = recs.iter().map(|r| r["kind"].as_str().unwrap()).collect();
        for k in [
            "signal",
            "position_open",
            "fill",
            "position_exit",
            "position_close",
        ] {
            assert!(kinds.contains(&k), "missing {k}: {kinds:?}");
        }
        let exit = recs.iter().find(|r| r["kind"] == "position_exit").unwrap();
        assert!(
            exit["pnl_sol"].as_f64().unwrap() > 0.0,
            "profitable round trip: {exit}"
        );
        let close = recs.iter().find(|r| r["kind"] == "position_close").unwrap();
        let shadows = close["shadows"].as_array().unwrap();
        assert_eq!(shadows.len(), 4, "live policy + 3 shadow policies scored");
    }

    #[tokio::test]
    async fn mirror_policy_follows_leader_sell_and_dev_dump_exits() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        let (leader, mint, creator) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let (mut e, _r) = engine(d, leader, "mirror_leader");
        e.on_tx(&trade_tx(
            100,
            Ev {
                mint,
                user: leader,
                creator,
                is_buy: true,
                sol: 2_000_000_000,
                tokens: 1,
                vsol: 30_000_000_000,
                vtok: VTOK,
                real_sol: 30_000_000_000,
            },
            0,
        ));
        let initial = e.positions[&mint].initial_tokens;
        // leader sells half of their bag → we sell half of ours
        e.on_tx(&trade_tx(
            101,
            Ev {
                mint,
                user: leader,
                creator,
                is_buy: false,
                sol: 1_000_000_000,
                tokens: 50,
                vsol: 31_000_000_000,
                vtok: VTOK,
                real_sol: 31_000_000_000,
            },
            100,
        ));
        let held = e.positions[&mint].held_tokens;
        assert!((held as f64 / initial as f64 - 0.5).abs() < 0.001);

        // second token: dev dump triggers ladder_trail's exit_on_dev_sell
        let (mint2, dev) = (Pubkey::new_unique(), Pubkey::new_unique());
        e.leaders.get_mut(&leader).unwrap().exit = Some("ladder_trail".into());
        e.on_tx(&trade_tx(
            102,
            Ev {
                mint: mint2,
                user: leader,
                creator: dev,
                is_buy: true,
                sol: 1_000_000_000,
                tokens: 1,
                vsol: 30_000_000_000,
                vtok: VTOK,
                real_sol: 30_000_000_000,
            },
            0,
        ));
        assert!(e.positions.contains_key(&mint2));
        e.on_tx(&trade_tx(
            103,
            Ev {
                mint: mint2,
                user: dev,
                creator: dev,
                is_buy: false,
                sol: 3_000_000_000,
                tokens: 1,
                vsol: 29_000_000_000,
                vtok: VTOK,
                real_sol: 29_000_000_000,
            },
            10,
        ));
        assert!(
            !e.positions.contains_key(&mint2),
            "dev sell exits immediately"
        );
    }

    #[tokio::test]
    async fn skips_are_recorded_with_reasons() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        let (leader, creator) = (Pubkey::new_unique(), Pubkey::new_unique());
        let (mut e, _r) = engine(d, leader, "ladder_trail");
        // too small leader buy
        e.on_tx(&trade_tx(
            100,
            Ev {
                mint: Pubkey::new_unique(),
                user: leader,
                creator,
                is_buy: true,
                sol: 100_000_000,
                tokens: 1,
                vsol: 30_000_000_000,
                vtok: VTOK,
                real_sol: 30_000_000_000,
            },
            0,
        ));
        // pool too thin (min_pool_sol = 15)
        e.on_tx(&trade_tx(
            101,
            Ev {
                mint: Pubkey::new_unique(),
                user: leader,
                creator,
                is_buy: true,
                sol: 1_000_000_000,
                tokens: 1,
                vsol: 30_000_000_000,
                vtok: VTOK,
                real_sol: 5_000_000_000,
            },
            0,
        ));
        // paused
        e.on_command(Command::Pause);
        e.on_tx(&trade_tx(
            102,
            Ev {
                mint: Pubkey::new_unique(),
                user: leader,
                creator,
                is_buy: true,
                sol: 1_000_000_000,
                tokens: 1,
                vsol: 30_000_000_000,
                vtok: VTOK,
                real_sol: 30_000_000_000,
            },
            0,
        ));
        // same signature twice is processed once
        e.on_tx(&trade_tx(
            102,
            Ev {
                mint: Pubkey::new_unique(),
                user: leader,
                creator,
                is_buy: true,
                sol: 1_000_000_000,
                tokens: 1,
                vsol: 30_000_000_000,
                vtok: VTOK,
                real_sol: 30_000_000_000,
            },
            0,
        ));
        assert!(e.positions.is_empty());
        assert_eq!(e.stats.signals, 3);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let reasons: Vec<String> = records(d)
            .iter()
            .filter(|r| r["kind"] == "signal")
            .map(|r| r["skip_reason"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(
            reasons,
            vec!["LeaderBuyTooSmall", "LiquidityTooLow", "KillSwitch"]
        );
    }

    #[tokio::test]
    async fn blacklist_and_one_entry_per_token() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        let (leader, bad_dev, good_dev) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let (mut e, _r) = engine(d, leader, "ladder_trail");
        e.cfg.engine.filters.one_entry_per_token = true;
        let buy = |slot, mint, creator| {
            trade_tx(
                slot,
                Ev {
                    mint,
                    user: leader,
                    creator,
                    is_buy: true,
                    sol: 1_000_000_000,
                    tokens: 1,
                    vsol: 30_000_000_000,
                    vtok: VTOK,
                    real_sol: 30_000_000_000,
                },
                0,
            )
        };
        let reply = e.on_command(Command::parse(&format!("blacklist {bad_dev}")).unwrap());
        assert!(reply.starts_with("blacklisted"), "{reply}");
        e.on_tx(&buy(200, Pubkey::new_unique(), bad_dev));
        assert!(
            e.positions.is_empty(),
            "token by a blacklisted dev must not be bought"
        );

        let mint = Pubkey::new_unique();
        e.on_tx(&buy(201, mint, good_dev));
        assert!(e.positions.contains_key(&mint));
        e.positions.clear(); // as if fully exited
        e.on_tx(&buy(202, mint, good_dev));
        assert!(
            e.positions.is_empty(),
            "second entry in the same token is skipped"
        );

        // runtime blacklist survives a restart
        let saved = std::fs::read_to_string(dir.path().join("blacklist.txt")).unwrap();
        assert_eq!(saved.trim(), bad_dev.to_string());
        let (e2, _r2) = engine(d, leader, "ladder_trail");
        assert!(e2.blacklist.contains(&bad_dev));

        tokio::time::sleep(Duration::from_millis(200)).await;
        let reasons: Vec<String> = records(d)
            .iter()
            .filter(|r| r["kind"] == "signal")
            .map(|r| r["skip_reason"].as_str().unwrap_or("-").to_string())
            .collect();
        assert_eq!(reasons, vec!["Blacklisted", "-", "AlreadyTraded"]);
    }

    #[test]
    fn ctl_parses_case_sensitive_addresses() {
        let pk = Pubkey::new_unique();
        assert_eq!(
            Command::parse(&format!("BlackList {pk}\n")),
            Some(Command::Blacklist(Some(pk)))
        );
        assert_eq!(
            Command::parse(&format!("unblacklist {pk}")),
            Some(Command::Unblacklist(pk))
        );
        assert_eq!(Command::parse("blacklist"), Some(Command::Blacklist(None)));
        assert_eq!(Command::parse("blacklist notanaddress"), None);
        assert_eq!(Command::parse("status extra"), None);
        assert_eq!(Command::parse("STATUS"), Some(Command::Status));
    }

    #[test]
    fn non_leader_wallets_are_ignored() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (mut e, _r) = engine(
                dir.path().to_str().unwrap(),
                Pubkey::new_unique(),
                "ladder_trail",
            );
            e.on_tx(&trade_tx(
                100,
                Ev {
                    mint: Pubkey::new_unique(),
                    user: Pubkey::new_unique(),
                    creator: Pubkey::new_unique(),
                    is_buy: true,
                    sol: 5_000_000_000,
                    tokens: 1,
                    vsol: 30_000_000_000,
                    vtok: VTOK,
                    real_sol: 30_000_000_000,
                },
                0,
            ));
            assert!(e.positions.is_empty());
            assert_eq!(e.stats.signals, 0);
        });
    }
}

#[cfg(test)]
mod adopt_tests {
    use super::*;

    #[tokio::test]
    async fn adopted_position_takes_entry_from_first_trade_then_stops_out() {
        let dir = tempfile::tempdir().unwrap();
        let s = include_str!("../../../../config/copybot.example.toml");
        let mut cfg = BotConfig::from_toml(s).unwrap();
        cfg.engine.mode = RunMode::Paper;
        let journal = Journal::start(dir.path().to_str().unwrap(), None, None)
            .await
            .unwrap();
        let (ftx, _f) = watch::channel(Filters::default());
        let (rtx, _r) = mpsc::channel(10);
        let mut e = Engine::new(cfg, Pubkey::new_unique(), None, None, journal, ftx, rtx);
        let mint = Pubkey::new_unique();
        e.adopt(mint, 1_000_000, chain::consts::TOKEN_PROGRAM, 6);
        e.apply_event(
            mint,
            MarketEvent::Price {
                t_ms: 1,
                price: 2.0,
            },
        );
        assert_eq!(e.positions[&mint].state.entry_price, 2.0);
        e.apply_event(
            mint,
            MarketEvent::Price {
                t_ms: 2,
                price: 1.2,
            },
        ); // -40% → stop loss (35%)
        assert!(!e.positions.contains_key(&mint));
    }
}

#[cfg(test)]
mod intel_gate_tests {
    use super::*;
    use crate::gmgn::{GmgnConfig, IntelGate};
    use chain::detect::Template;
    use chain::pump::{CurveCoin, CurveState};

    fn swap(leader: Pubkey, mint: Pubkey) -> DetectedSwap {
        DetectedSwap {
            wallet: leader,
            mint,
            side: Side::Buy,
            venue: Venue::PumpFunCurve,
            sol_amount: 1_000_000_000,
            token_amount: 1,
            token_decimals: 6,
            token_program: chain::consts::TOKEN_PROGRAM,
            price_sol: 3e-8,
            pool_sol: Some(30_000_000_000),
            fraction_sold: None,
            exact: true,
            template: Template::Curve {
                coin: CurveCoin::sol_paired(
                    mint,
                    Pubkey::new_unique(),
                    chain::consts::TOKEN_PROGRAM,
                    false,
                ),
                state: CurveState {
                    virtual_token_reserves: 1_000_000_000_000_000,
                    virtual_quote_reserves: 30_000_000_000,
                    real_token_reserves: 800_000_000_000_000,
                    real_quote_reserves: 30_000_000_000,
                },
                fee_bps: 125,
            },
            creator: None,
            migrated: false,
        }
    }

    async fn engine(allow_on_timeout: bool) -> (Engine, Pubkey) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg =
            BotConfig::from_toml(include_str!("../../../../config/copybot.example.toml")).unwrap();
        cfg.engine.mode = RunMode::Paper;
        let leader = Pubkey::new_unique();
        cfg.engine.leaders = vec![LeaderConfig {
            address: leader.to_string(),
            label: "L".into(),
            enabled: true,
            copy_pct: None,
            max_buy_sol: None,
            exit: None,
        }];
        cfg.infra.gmgn = Some(GmgnConfig {
            api_key_env: "X".into(),
            requests_per_sec: 100.0,
            enrich: true,
            gate: Some(IntelGate {
                timeout_ms: 50,
                allow_on_timeout,
                max_top10_rate: Some(0.5),
                max_dev_team_hold_rate: None,
                max_creator_hold_rate: None,
                max_insider_hold_rate: None,
                max_bundler_rate: Some(0.3),
                max_rat_trader_rate: None,
                max_rug_ratio: None,
                max_sniper_count: None,
                max_fresh_wallet_rate: None,
                min_holders: None,
                skip_wash_trading: true,
                require_mint_renounced: false,
                require_freeze_renounced: false,
                skip_if_dev_sold: false,
            }),
        });
        let journal = Journal::start(dir.path().to_str().unwrap(), None, None)
            .await
            .unwrap();
        std::mem::forget(dir);
        let (ftx, _f) = watch::channel(Filters::default());
        let (rtx, _r) = mpsc::channel(10);
        (
            Engine::new(cfg, Pubkey::new_unique(), None, None, journal, ftx, rtx),
            leader,
        )
    }

    fn pend(e: &mut Engine, leader: Pubkey, mint: Pubkey) {
        let lc = e.leaders[&leader].clone();
        e.tokens.insert(
            mint,
            TokenInfo {
                template: swap(leader, mint).template,
                price: 3e-8,
                pool_sol: None,
                creator: None,
                token_program: chain::consts::TOKEN_PROGRAM,
                decimals: 6,
                created_ms: None,
                migrated: false,
            },
        );
        e.pending.insert(
            mint,
            PendingEntry {
                observed_ns: ChainTx::now_ns(),
                swap: swap(leader, mint),
                leader: lc,
                lamports: 100_000_000,
                signal: json!({}),
            },
        );
    }

    #[tokio::test]
    async fn gate_blocks_concentrated_token_and_passes_clean_one() {
        let (mut e, leader) = engine(false).await;
        let (bad, good) = (Pubkey::new_unique(), Pubkey::new_unique());
        pend(&mut e, leader, bad);
        e.on_intel(
            bad,
            Some(Box::new((
                TokenIntel {
                    top10_rate: Some(0.8),
                    ..Default::default()
                },
                json!({}),
            ))),
            None,
        );
        assert!(!e.positions.contains_key(&bad));
        assert_eq!(e.stats.skip_reasons.get("IntelGate"), Some(&1));
        pend(&mut e, leader, good);
        e.on_intel(
            good,
            Some(Box::new((
                TokenIntel {
                    top10_rate: Some(0.2),
                    bundler_rate: Some(0.1),
                    ..Default::default()
                },
                json!({}),
            ))),
            None,
        );
        assert!(e.positions.contains_key(&good));
    }

    #[tokio::test]
    async fn timeout_policy() {
        let (mut e, leader) = engine(false).await;
        let m = Pubkey::new_unique();
        pend(&mut e, leader, m);
        e.on_intel(m, None, Some("timeout".into()));
        assert!(
            !e.positions.contains_key(&m),
            "skip when data is missing and allow_on_timeout = false"
        );

        let (mut e, leader) = engine(true).await;
        pend(&mut e, leader, m);
        e.on_intel(m, None, Some("timeout".into()));
        assert!(
            e.positions.contains_key(&m),
            "enter when allow_on_timeout = true"
        );
    }
}
