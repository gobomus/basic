//! Order construction and delivery.
//!
//! Hot path for a copy buy (live mode), all in-process:
//!   template from the leader's event → instructions → sign with cached
//!   blockhash → one signature fanned out to every sender.
//! No RPC round trip is needed for Pump curve or PumpSwap entries.

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use chain::detect::Template;
use chain::meteora_dbc;
use chain::nonce::NoncePool;
use chain::pda;
use chain::pump::{self};
use chain::pump_amm::{self, AmmCoin, GlobalConfig, Pool};
use chain::rpc::Rpc;
use chain::sender::{Fanout, SendReport};
use chain::solana_sdk::hash::Hash;
use chain::solana_sdk::instruction::Instruction;
use chain::solana_sdk::pubkey::Pubkey;
use chain::solana_sdk::signature::{Keypair, Signer};
use chain::solana_sdk::transaction::VersionedTransaction;
use chain::tx::{self, FeePlan};
use chain::{consts::*, ixs};
use serde::Serialize;
use tokio::sync::RwLock;

use crate::cfg::{FeeConfig, JupiterConfig};

#[derive(Debug, Clone, Serialize)]
pub struct SendOutcome {
    /// First variant's signature (the landed one is reported by `resolve`).
    pub signature: String,
    /// All variants (one per tip group when a durable nonce is used).
    pub signatures: Vec<String>,
    pub build_us: u64,
    pub reports: Vec<SendReport>,
    pub tip_lamports: u64,
    /// Durable nonce account the variants share, if any.
    pub nonce_account: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Confirmation {
    Landed { slot: u64, signature: String },
    Failed { slot: u64, err: String },
    Expired,
}

pub struct Exec {
    pub rpc: Rpc,
    pub fanout: Fanout,
    pub kp: Arc<Keypair>,
    pub fees: FeeConfig,
    pub tip_accounts: Vec<Pubkey>,
    pub blockhash: Arc<RwLock<(Hash, Instant)>>,
    pub amm_global: Arc<RwLock<Option<GlobalConfig>>>,
    pub jupiter: Option<JupiterConfig>,
    pub nonces: Arc<NoncePool>,
    http: reqwest::Client,
}

impl Exec {
    pub fn new(
        rpc: Rpc,
        fanout: Fanout,
        kp: Arc<Keypair>,
        fees: FeeConfig,
        tip_accounts: Vec<Pubkey>,
        jupiter: Option<JupiterConfig>,
        nonce_accounts: Vec<Pubkey>,
    ) -> Self {
        Self {
            rpc,
            fanout,
            kp,
            fees,
            tip_accounts,
            blockhash: Arc::new(RwLock::new((
                Hash::default(),
                Instant::now() - Duration::from_secs(3600),
            ))),
            amm_global: Arc::new(RwLock::new(None)),
            jupiter,
            nonces: Arc::new(NoncePool::new(&nonce_accounts)),
            http: chain::rpc::http_client(Duration::from_secs(5)),
        }
    }

    pub fn me(&self) -> Pubkey {
        self.kp.pubkey()
    }

    /// Keep a fresh blockhash and the PumpSwap global config in memory.
    pub fn spawn_refreshers(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                match me.rpc.latest_blockhash("processed").await {
                    Ok((h, _)) => *me.blockhash.write().await = (h, Instant::now()),
                    Err(e) => tracing::warn!("blockhash refresh: {e}"),
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        });
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                match me.rpc.account(&pda::amm_global_config()).await {
                    Ok(Some(a)) => match GlobalConfig::decode(&a.data) {
                        Ok(g) => *me.amm_global.write().await = Some(g),
                        Err(e) => tracing::warn!("PumpSwap GlobalConfig decode: {e}"),
                    },
                    Ok(None) => tracing::warn!("PumpSwap GlobalConfig not found"),
                    Err(e) => tracing::warn!("PumpSwap GlobalConfig fetch: {e}"),
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        let me = self.clone();
        tokio::spawn(async move {
            for acc in me.nonces.accounts() {
                me.refresh_nonce(&acc).await;
            }
            tracing::info!(
                "durable nonces ready: {}/{}",
                me.nonces.available(),
                me.nonces.accounts().len()
            );
        });
    }

    async fn fresh_blockhash(&self) -> anyhow::Result<Hash> {
        let (h, at) = *self.blockhash.read().await;
        if at.elapsed() < Duration::from_secs(20) && h != Hash::default() {
            return Ok(h);
        }
        let (h, _) = self.rpc.latest_blockhash("processed").await?;
        *self.blockhash.write().await = (h, Instant::now());
        Ok(h)
    }

    // ---------------------------------------------------------------- instruction plans

    /// Instructions to buy `sol_in` lamports worth. Never spends more than `sol_in`
    /// (plus fees/tip/rent). Returns (instructions, expected tokens, min tokens).
    pub fn buy_ixs(
        &self,
        tpl: &Template,
        sol_in: u64,
        slippage_bps: u32,
        salt: u64,
    ) -> anyhow::Result<(Vec<Instruction>, u64, u64)> {
        let me = self.me();
        match tpl {
            Template::Curve {
                coin,
                state,
                fee_bps,
            } => {
                let expected = pump::buy_tokens_for_quote(state, sol_in, *fee_bps);
                anyhow::ensure!(expected > 0, "curve quote is zero (curve complete?)");
                let min_out = pump::apply_slippage_down(expected, slippage_bps);
                let ixs = vec![
                    ixs::create_ata_idempotent(&me, &me, &coin.mint, &coin.base_token_program),
                    pump::buy_exact_quote_in_v2(coin, &me, sol_in, min_out, salt),
                ];
                Ok((ixs, expected, min_out))
            }
            Template::Amm {
                coin,
                base_reserve,
                quote_reserve,
                fee_bps,
            } => {
                let expected =
                    pump_amm::buy_base_for_quote(*base_reserve, *quote_reserve, sol_in, *fee_bps);
                anyhow::ensure!(expected > 0, "pool quote is zero");
                // Spend exactly sol_in; fail if fewer tokens than the slippage floor come back.
                let min_out = pump::apply_slippage_down(expected, slippage_bps);
                Ok((
                    pump_amm::buy_instructions(coin, &me, sol_in, min_out),
                    expected,
                    min_out,
                ))
            }
            Template::Dbc {
                coin,
                price_raw,
                fee_bps,
            } => {
                let expected = meteora_dbc::estimate_buy(*price_raw, sol_in, *fee_bps);
                anyhow::ensure!(expected > 0, "DBC quote is zero");
                let min_out = pump::apply_slippage_down(expected, slippage_bps);
                Ok((
                    meteora_dbc::buy_instructions(coin, &me, sol_in, min_out),
                    expected,
                    min_out,
                ))
            }
            Template::LaunchLab {
                coin,
                price_raw,
                fee_bps,
            } => {
                let expected = meteora_dbc::estimate_buy(*price_raw, sol_in, *fee_bps);
                anyhow::ensure!(expected > 0, "LaunchLab quote is zero");
                let min_out = pump::apply_slippage_down(expected, slippage_bps);
                Ok((
                    chain::raydium_launchlab::buy_instructions(coin, &me, sol_in, min_out),
                    expected,
                    min_out,
                ))
            }
            Template::Generic => anyhow::bail!("generic venue: use the Jupiter route"),
        }
    }

    /// Instructions to sell `tokens`. `close` also closes the token account (full exit).
    #[allow(clippy::too_many_arguments)]
    pub fn sell_ixs(
        &self,
        tpl: &Template,
        token_program: &Pubkey,
        mint: &Pubkey,
        tokens: u64,
        slippage_bps: u32,
        close: bool,
        salt: u64,
    ) -> anyhow::Result<(Vec<Instruction>, u64)> {
        let me = self.me();
        let (mut ixs, expected) = match tpl {
            Template::Curve {
                coin,
                state,
                fee_bps,
            } => {
                let expected = pump::sell_quote_for_tokens(state, tokens, *fee_bps);
                let min_out = pump::apply_slippage_down(expected, slippage_bps);
                (
                    vec![pump::sell_v2(coin, &me, tokens, min_out, salt)],
                    expected,
                )
            }
            Template::Amm {
                coin,
                base_reserve,
                quote_reserve,
                fee_bps,
            } => {
                let expected =
                    pump_amm::sell_quote_for_base(*base_reserve, *quote_reserve, tokens, *fee_bps);
                let min_out = pump::apply_slippage_down(expected, slippage_bps);
                (
                    pump_amm::sell_instructions(coin, &me, tokens, min_out),
                    expected,
                )
            }
            Template::Dbc {
                coin,
                price_raw,
                fee_bps,
            } => {
                let expected = meteora_dbc::estimate_sell(*price_raw, tokens, *fee_bps);
                let min_out = pump::apply_slippage_down(expected, slippage_bps);
                (
                    meteora_dbc::sell_instructions(coin, &me, tokens, min_out),
                    expected,
                )
            }
            Template::LaunchLab {
                coin,
                price_raw,
                fee_bps,
            } => {
                let expected = meteora_dbc::estimate_sell(*price_raw, tokens, *fee_bps);
                let min_out = pump::apply_slippage_down(expected, slippage_bps);
                (
                    chain::raydium_launchlab::sell_instructions(coin, &me, tokens, min_out),
                    expected,
                )
            }
            Template::Generic => anyhow::bail!("generic venue: use the Jupiter route"),
        };
        if close {
            ixs.push(ixs::close_token_account(
                &pda::ata(&me, mint, token_program),
                &me,
                &me,
                token_program,
            ));
        }
        Ok((ixs, expected))
    }

    /// Template for a coin that graduated from the curve to its canonical PumpSwap pool.
    pub async fn migrated_template(
        &self,
        mint: &Pubkey,
        base_token_program: &Pubkey,
        salt: u64,
    ) -> anyhow::Result<Template> {
        let pool_key = pda::canonical_pump_pool(mint);
        let g = self
            .amm_global
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("PumpSwap GlobalConfig not loaded yet"))?;
        let pool_acc = self
            .rpc
            .account(&pool_key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("canonical pool {pool_key} not found"))?;
        let pool = Pool::decode(&pool_acc.data)?;
        let coin = AmmCoin::from_pool(
            pool_key,
            &pool,
            *base_token_program,
            TOKEN_PROGRAM,
            &g,
            salt,
        );
        let accs = self
            .rpc
            .accounts(&[pool.pool_base_token_account, pool.pool_quote_token_account])
            .await?;
        let amt = |a: &Option<chain::rpc::AccountData>| -> u64 {
            a.as_ref()
                .and_then(|a| a.data.get(64..72))
                .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(0)
        };
        let base_reserve = amt(&accs[0]);
        let quote_reserve = (amt(&accs[1]) as i128 + pool.virtual_quote_reserves).max(0) as u128;
        let fee_bps =
            g.lp_fee_basis_points + g.protocol_fee_basis_points + g.coin_creator_fee_basis_points;
        Ok(Template::Amm {
            coin,
            base_reserve,
            quote_reserve,
            fee_bps,
        })
    }

    // ---------------------------------------------------------------- sending

    pub async fn send_ixs(
        &self,
        body: Vec<Instruction>,
        cu_limit: u32,
        tip: u64,
        salt: u64,
    ) -> anyhow::Result<SendOutcome> {
        let t = Instant::now();
        let groups = self.fanout.groups(&self.tip_accounts);
        let fees = |min_tip: u64| FeePlan {
            cu_limit,
            cu_price_micro_lamports: self.fees.cu_price_micro_lamports,
            tip_lamports: if tip > 0 { tip.max(min_tip) } else { 0 },
        };
        let pick = |g: &chain::sender::TipGroup| -> Option<Pubkey> {
            (!g.tip_accounts.is_empty())
                .then(|| g.tip_accounts[(salt as usize) % g.tip_accounts.len()])
        };

        // Several tip families + a free nonce: one variant per family, mutually
        // exclusive on-chain because they all advance the same nonce.
        if groups.len() > 1 {
            if let Some((nonce_acc, nonce_val)) = self.nonces.take() {
                let mut variants = Vec::with_capacity(groups.len());
                for g in &groups {
                    let tip_acc = pick(g);
                    let mut fp = fees(g.min_tip_lamports);
                    if tip_acc.is_none() {
                        fp.tip_lamports = 0;
                    }
                    match tx::build_with_nonce(
                        &self.kp,
                        body.clone(),
                        fp,
                        tip_acc.as_ref(),
                        &nonce_acc,
                        nonce_val,
                    ) {
                        Ok(sg) => variants.push((g.senders.clone(), sg)),
                        Err(e) => {
                            self.nonces.set(&nonce_acc, nonce_val);
                            return Err(e);
                        }
                    }
                }
                let build_us = t.elapsed().as_micros() as u64;
                let sends = variants
                    .iter()
                    .map(|(idx, sg)| self.fanout.send_to(idx, &sg.wire_b64));
                let reports = futures::future::join_all(sends)
                    .await
                    .into_iter()
                    .flatten()
                    .collect();
                let signatures: Vec<String> = variants
                    .iter()
                    .map(|(_, sg)| sg.signature.to_string())
                    .collect();
                return Ok(SendOutcome {
                    signature: signatures[0].clone(),
                    signatures,
                    build_us,
                    reports,
                    tip_lamports: tip,
                    nonce_account: Some(nonce_acc.to_string()),
                });
            }
            tracing::debug!("no free durable nonce: single variant to the first tip group");
        }

        // Single variant on a recent blockhash → first tip group (+ plain endpoints).
        let bh = self.fresh_blockhash().await?;
        let g = &groups[0];
        let signed = tx::build(
            &self.kp,
            body,
            fees(g.min_tip_lamports),
            pick(g).as_ref(),
            bh,
        )?;
        let build_us = t.elapsed().as_micros() as u64;
        let reports = self.fanout.send_to(&g.senders, &signed.wire_b64).await;
        let sig = signed.signature.to_string();
        Ok(SendOutcome {
            signature: sig.clone(),
            signatures: vec![sig],
            build_us,
            reports,
            tip_lamports: tip,
            nonce_account: None,
        })
    }

    /// Wait for any variant to land. With a durable nonce, an order that has
    /// not landed by `timeout` is cancelled by advancing the nonce ourselves,
    /// so a stale variant can never execute later. The slot is then refreshed.
    pub async fn resolve(&self, o: &SendOutcome, timeout: Duration) -> Confirmation {
        let mut c = self.confirm_any(&o.signatures, timeout).await;
        if let Some(acc) = o
            .nonce_account
            .as_ref()
            .and_then(|a| a.parse::<Pubkey>().ok())
        {
            if c == Confirmation::Expired {
                match self.cancel_nonce(&acc).await {
                    Ok(()) => tracing::info!(
                        "order {} expired: nonce advanced, variants invalidated",
                        o.signature
                    ),
                    Err(e) => tracing::warn!("nonce cancel for {} failed: {e}", o.signature),
                }
                // a variant may have landed while we were cancelling
                c = self
                    .confirm_any(&o.signatures, Duration::from_millis(300))
                    .await;
            }
            self.refresh_nonce(&acc).await;
        }
        c
    }

    async fn confirm_any(&self, sigs: &[String], timeout: Duration) -> Confirmation {
        let start = Instant::now();
        loop {
            if let Ok(st) = self.rpc.signature_statuses(sigs).await {
                for (i, s) in st.iter().enumerate() {
                    if let Some((slot, err)) = s {
                        return match err {
                            None => Confirmation::Landed {
                                slot: *slot,
                                signature: sigs[i].clone(),
                            },
                            Some(e) => Confirmation::Failed {
                                slot: *slot,
                                err: e.to_string(),
                            },
                        };
                    }
                }
            }
            if start.elapsed() >= timeout {
                return Confirmation::Expired;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn cancel_nonce(&self, acc: &Pubkey) -> anyhow::Result<()> {
        let bh = self.fresh_blockhash().await?;
        let fp = FeePlan {
            cu_limit: 20_000,
            cu_price_micro_lamports: self.fees.cu_price_micro_lamports,
            tip_lamports: 0,
        };
        let signed = tx::build(
            &self.kp,
            vec![chain::nonce::advance_nonce(acc, &self.me())],
            fp,
            None,
            bh,
        )?;
        self.rpc.send(&signed.wire_b64).await?;
        match self
            .confirm_any(&[signed.signature.to_string()], Duration::from_secs(15))
            .await
        {
            Confirmation::Landed { .. } => Ok(()),
            other => anyhow::bail!("cancel not confirmed: {other:?}"),
        }
    }

    /// Re-read a nonce account and return it to the pool.
    pub async fn refresh_nonce(&self, acc: &Pubkey) {
        for _ in 0..5 {
            if let Ok(Some(a)) = self.rpc.account(acc).await {
                if let Some((auth, h)) = chain::nonce::parse_nonce_account(&a.data) {
                    if auth == self.me() {
                        self.nonces.set(acc, h);
                    } else {
                        tracing::error!(
                            "nonce {acc} authority is {auth}, not the hot wallet — slot disabled"
                        );
                    }
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        tracing::warn!("nonce {acc} could not be refreshed; slot stays out of rotation");
    }

    // ---------------------------------------------------------------- Jupiter fallback

    /// Swap through Jupiter (venues without a direct builder). `amount` is in
    /// input-mint base units. Returns the send outcome.
    pub async fn jupiter_swap(
        &self,
        input: &Pubkey,
        output: &Pubkey,
        amount: u64,
        slippage_bps: u32,
    ) -> anyhow::Result<SendOutcome> {
        let j = self
            .jupiter
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no [infra.jupiter] configured"))?;
        let key = j.api_key_env.as_ref().and_then(|e| std::env::var(e).ok());
        let t = Instant::now();
        let mut q = self.http.get(format!("{}/quote", j.base_url)).query(&[
            ("inputMint", input.to_string()),
            ("outputMint", output.to_string()),
            ("amount", amount.to_string()),
            ("slippageBps", slippage_bps.to_string()),
        ]);
        if let Some(k) = &key {
            q = q.header("x-api-key", k);
        }
        let quote: serde_json::Value = q.send().await?.error_for_status()?.json().await?;
        let mut s = self.http.post(format!("{}/swap", j.base_url)).json(&serde_json::json!({
            "quoteResponse": quote,
            "userPublicKey": self.me().to_string(),
            "wrapAndUnwrapSol": true,
            "dynamicComputeUnitLimit": true,
            "prioritizationFeeLamports": self.fees.cu_price_micro_lamports.saturating_mul(self.fees.cu_limit_buy as u64) / 1_000_000,
        }));
        if let Some(k) = &key {
            s = s.header("x-api-key", k);
        }
        let swap: serde_json::Value = s.send().await?.error_for_status()?.json().await?;
        let b64 = swap["swapTransaction"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("jupiter: no swapTransaction: {swap}"))?;
        let mut vtx: VersionedTransaction =
            bincode::deserialize(&base64::engine::general_purpose::STANDARD.decode(b64)?)?;
        let sig = self.kp.sign_message(&vtx.message.serialize());
        anyhow::ensure!(
            vtx.message.static_account_keys().first() == Some(&self.me()),
            "jupiter tx payer mismatch"
        );
        vtx.signatures[0] = sig;
        let wire = base64::engine::general_purpose::STANDARD.encode(bincode::serialize(&vtx)?);
        let build_us = t.elapsed().as_micros() as u64;
        let reports = self.fanout.send(&wire).await;
        Ok(SendOutcome {
            signature: sig.to_string(),
            signatures: vec![sig.to_string()],
            build_us,
            reports,
            tip_lamports: 0,
            nonce_account: None,
        })
    }
}

/// The wrapped-SOL mint, re-exported for callers routing through Jupiter.
pub const SOL_MINT: Pubkey = WSOL_MINT;
