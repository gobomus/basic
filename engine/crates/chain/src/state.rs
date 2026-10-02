//! A coin's live pool state, read straight from its accounts (no transaction
//! needed). This is how the free feed prices open positions: one batched
//! `getMultipleAccounts` per tick covers every coin we watch.

use solana_sdk::pubkey::Pubkey;

use crate::consts::{TOKEN_PROGRAM, WSOL_MINT};
use crate::detect::Template;
use crate::pda;
use crate::pump::{self, BondingCurve};
use crate::pump_amm::{AmmCoin, GlobalConfig, Pool};
use crate::rpc::{AccountData, Rpc};

/// One coin under watch.
#[derive(Debug, Clone)]
pub struct Watch {
    pub mint: Pubkey,
    pub template: Template,
    pub decimals: u8,
    pub token_program: Pubkey,
    /// A position is open (poll fast); otherwise it is only being tracked for scoring.
    pub held: bool,
}

/// Venues whose state we can read from accounts (Pump curve, PumpSwap).
pub fn supported(t: &Template) -> bool {
    matches!(t, Template::Curve { .. } | Template::Amm { .. })
}

/// Accounts to read for `refresh`, in order.
pub fn accounts_of(t: &Template) -> Vec<Pubkey> {
    match t {
        Template::Curve { coin, .. } => vec![pda::bonding_curve(&coin.mint)],
        Template::Amm { coin, .. } => vec![
            coin.pool,
            coin.pool_base_token_account,
            coin.pool_quote_token_account,
        ],
        _ => vec![],
    }
}

#[derive(Debug, Clone)]
pub struct Refreshed {
    pub template: Template,
    pub price_sol: f64,
    pub pool_sol: u64,
    /// Curve only: the bonding curve is complete (the coin moved to PumpSwap).
    pub complete: bool,
}

/// Amount field of an SPL token account (Token and Token-2022 share the layout).
fn token_amount(a: &Option<AccountData>) -> Option<u64> {
    a.as_ref()
        .and_then(|a| a.data.get(64..72))
        .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
}

/// Rebuild `t` from freshly read accounts (same order as `accounts_of`).
pub fn refresh(t: &Template, decimals: u8, accs: &[Option<AccountData>]) -> Option<Refreshed> {
    match t {
        Template::Curve { coin, fee_bps, .. } => {
            let bc = BondingCurve::decode(&accs.first()?.as_ref()?.data).ok()?;
            if bc.quote_mint != Pubkey::default() && bc.quote_mint != WSOL_MINT {
                return None; // not SOL-paired
            }
            let price = pump::spot_price(
                bc.state.virtual_quote_reserves,
                bc.state.virtual_token_reserves,
            );
            Some(Refreshed {
                template: Template::Curve {
                    coin: coin.clone(),
                    state: bc.state,
                    fee_bps: *fee_bps,
                },
                price_sol: price,
                pool_sol: bc.state.real_quote_reserves,
                complete: bc.complete,
            })
        }
        Template::Amm { coin, fee_bps, .. } => {
            let pool = Pool::decode(&accs.first()?.as_ref()?.data).ok()?;
            if pool.quote_mint != WSOL_MINT {
                return None;
            }
            let base = token_amount(accs.get(1)?)?;
            let quote = token_amount(accs.get(2)?)?;
            Some(amm_refreshed(coin, *fee_bps, decimals, base, quote, &pool))
        }
        _ => None,
    }
}

fn amm_refreshed(
    coin: &AmmCoin,
    fee_bps: u64,
    decimals: u8,
    base: u64,
    quote: u64,
    pool: &Pool,
) -> Refreshed {
    let quote_reserve = (quote as i128 + pool.virtual_quote_reserves).max(0) as u128;
    let price = if base == 0 {
        0.0
    } else {
        (quote_reserve as f64 / 1e9) / (base as f64 / 10f64.powi(decimals as i32))
    };
    Refreshed {
        template: Template::Amm {
            coin: coin.clone(),
            base_reserve: base,
            quote_reserve,
            fee_bps,
        },
        price_sol: price,
        pool_sol: quote_reserve as u64,
        complete: false,
    }
}

/// Read the current state of several coins at once (one RPC call for all).
/// Returns the slot of the read and, per input, `None` when the venue is not
/// readable from accounts or an account is missing.
pub async fn read(
    rpc: &Rpc,
    items: &[(Template, u8)],
) -> anyhow::Result<(u64, Vec<Option<Refreshed>>)> {
    let mut keys = Vec::new();
    let mut spans = Vec::with_capacity(items.len());
    for (t, _) in items {
        let k = accounts_of(t);
        spans.push((keys.len(), k.len()));
        keys.extend(k);
    }
    if keys.is_empty() {
        return Ok((0, vec![None; items.len()]));
    }
    // no rate-limit backoff: the poller retries on its next tick, and a stalled
    // read would delay exits more than a skipped one
    let (slot, accs) = rpc.accounts_at_fast(&keys).await?;
    let out = items
        .iter()
        .zip(spans)
        .map(|((t, d), (off, n))| {
            (n > 0)
                .then(|| accs.get(off..off + n))
                .flatten()
                .and_then(|a| refresh(t, *d, a))
        })
        .collect();
    Ok((slot, out))
}

pub async fn load_amm_global(rpc: &Rpc) -> anyhow::Result<GlobalConfig> {
    let a = rpc
        .account(&pda::amm_global_config())
        .await?
        .ok_or_else(|| anyhow::anyhow!("PumpSwap GlobalConfig not found"))?;
    Ok(GlobalConfig::decode(&a.data)?)
}

/// Template for a coin that graduated from the curve to its canonical PumpSwap pool.
pub async fn amm_template(
    rpc: &Rpc,
    g: &GlobalConfig,
    mint: &Pubkey,
    base_token_program: &Pubkey,
    decimals: u8,
    salt: u64,
) -> anyhow::Result<(Template, Refreshed)> {
    let pool_key = pda::canonical_pump_pool(mint);
    let pool_acc = rpc
        .account(&pool_key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("canonical pool {pool_key} not found"))?;
    let pool = Pool::decode(&pool_acc.data)?;
    let coin = AmmCoin::from_pool(pool_key, &pool, *base_token_program, TOKEN_PROGRAM, g, salt);
    let accs = rpc
        .accounts(&[pool.pool_base_token_account, pool.pool_quote_token_account])
        .await?;
    let (base, quote) = (
        token_amount(&accs[0]).unwrap_or(0),
        token_amount(&accs[1]).unwrap_or(0),
    );
    let fee_bps =
        g.lp_fee_basis_points + g.protocol_fee_basis_points + g.coin_creator_fee_basis_points;
    let r = amm_refreshed(&coin, fee_bps, decimals, base, quote, &pool);
    Ok((r.template.clone(), r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pump::{CurveCoin, CurveState};

    fn acc(data: Vec<u8>) -> Option<AccountData> {
        Some(AccountData {
            owner: Pubkey::new_unique(),
            lamports: 1,
            data,
        })
    }

    fn curve_account(s: CurveState, complete: bool) -> Vec<u8> {
        let mut d = vec![0u8; 8];
        for v in [
            s.virtual_token_reserves,
            s.virtual_quote_reserves,
            s.real_token_reserves,
            s.real_quote_reserves,
            1_000_000_000_000_000,
        ] {
            d.extend(v.to_le_bytes());
        }
        d.push(complete as u8);
        d.extend(Pubkey::new_unique().to_bytes()); // creator
        d
    }

    fn pool_account(virtual_quote: i128) -> Vec<u8> {
        let mut d = vec![0u8; 8 + 1];
        d.extend(0u16.to_le_bytes());
        d.extend(Pubkey::new_unique().to_bytes()); // creator
        d.extend(Pubkey::new_unique().to_bytes()); // base mint
        d.extend(WSOL_MINT.to_bytes()); // quote mint
        d.extend(Pubkey::new_unique().to_bytes()); // lp mint
        d.extend(Pubkey::new_unique().to_bytes()); // base vault
        d.extend(Pubkey::new_unique().to_bytes()); // quote vault
        d.extend(0u64.to_le_bytes()); // lp supply
                                      // trailing fields added by later program versions (present on live pools)
        d.extend(Pubkey::new_unique().to_bytes()); // coin_creator
        d.push(0); // is_mayhem_mode
        d.push(0); // is_cashback_coin
        d.extend(virtual_quote.to_le_bytes()); // virtual_quote_reserves
        d
    }

    fn vault(amount: u64) -> Vec<u8> {
        let mut d = vec![0u8; 64];
        d.extend(amount.to_le_bytes());
        d.extend([0u8; 29]);
        d
    }

    fn curve_template() -> Template {
        Template::Curve {
            coin: CurveCoin::sol_paired(
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                TOKEN_PROGRAM,
                false,
            ),
            state: CurveState {
                virtual_token_reserves: 1,
                virtual_quote_reserves: 1,
                real_token_reserves: 1,
                real_quote_reserves: 1,
            },
            fee_bps: 125,
        }
    }

    #[test]
    fn curve_refresh_reads_reserves_and_price() {
        let s = CurveState {
            virtual_token_reserves: 800_000_000_000_000,
            virtual_quote_reserves: 40_000_000_000,
            real_token_reserves: 500_000_000_000_000,
            real_quote_reserves: 10_000_000_000,
        };
        let r = refresh(&curve_template(), 6, &[acc(curve_account(s, false))]).unwrap();
        assert!(!r.complete);
        assert_eq!(r.pool_sol, 10_000_000_000);
        assert!((r.price_sol - 40.0 / 800_000_000.0).abs() < 1e-15);
        match r.template {
            Template::Curve { state, fee_bps, .. } => {
                assert_eq!(state, s);
                assert_eq!(fee_bps, 125);
            }
            _ => panic!("still a curve"),
        }
        let done = refresh(&curve_template(), 6, &[acc(curve_account(s, true))]).unwrap();
        assert!(done.complete);
        assert!(refresh(&curve_template(), 6, &[None]).is_none());
    }

    #[test]
    fn amm_refresh_adds_virtual_quote_reserves() {
        let coin = AmmCoin {
            pool: Pubkey::new_unique(),
            base_mint: Pubkey::new_unique(),
            quote_mint: WSOL_MINT,
            pool_base_token_account: Pubkey::new_unique(),
            pool_quote_token_account: Pubkey::new_unique(),
            base_token_program: TOKEN_PROGRAM,
            quote_token_program: TOKEN_PROGRAM,
            coin_creator: Pubkey::new_unique(),
            is_mayhem_mode: false,
            is_cashback_coin: false,
            protocol_fee_recipient: Pubkey::new_unique(),
            buyback_fee_recipient: Pubkey::new_unique(),
        };
        let t = Template::Amm {
            coin,
            base_reserve: 1,
            quote_reserve: 1,
            fee_bps: 100,
        };
        // 200M tokens (6 dp) against 60 SOL real + 30 SOL virtual
        let accs = [
            acc(pool_account(30_000_000_000)),
            acc(vault(200_000_000_000_000)),
            acc(vault(60_000_000_000)),
        ];
        let r = refresh(&t, 6, &accs).unwrap();
        assert_eq!(r.pool_sol, 90_000_000_000);
        assert!((r.price_sol - 90.0 / 200_000_000.0).abs() < 1e-15);
        match r.template {
            Template::Amm {
                base_reserve,
                quote_reserve,
                fee_bps,
                ..
            } => {
                assert_eq!(base_reserve, 200_000_000_000_000);
                assert_eq!(quote_reserve, 90_000_000_000);
                assert_eq!(fee_bps, 100);
            }
            _ => panic!("still an amm"),
        }
        assert!(refresh(&t, 6, &accs[..2]).is_none());
    }

    #[test]
    fn watched_accounts_follow_the_template() {
        assert_eq!(accounts_of(&curve_template()).len(), 1);
        assert!(supported(&curve_template()));
        assert!(!supported(&Template::Generic));
        assert!(accounts_of(&Template::Generic).is_empty());
    }
}
