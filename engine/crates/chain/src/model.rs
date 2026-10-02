//! Normalised transaction, built from either a live Geyser update or an RPC
//! `getTransaction` (json encoding) response, so detection logic is shared by
//! the live engine, backfills and tests.

use std::collections::HashMap;

use serde_json::Value;
use solana_sdk::pubkey::Pubkey;

use engine_core::types::FeedSource;

#[derive(Debug, Clone, PartialEq)]
pub struct Ix {
    pub program: Pubkey,
    pub accounts: Vec<Pubkey>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TokenBal {
    pub account: Pubkey,
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub program: Pubkey,
    pub amount: u64,
    pub decimals: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChainTx {
    pub signature: String,
    pub slot: u64,
    pub tx_index: Option<u32>,
    pub block_time_ms: Option<i64>,
    pub observed_at_ns: u64,
    /// RPC feeds only: when the whole transaction was in hand (0 = not applicable).
    pub fetched_at_ns: u64,
    /// RPC feeds only: `getTransaction` attempts it took.
    pub fetch_tries: u32,
    pub source: FeedSource,
    pub failed: bool,
    pub fee: u64,
    /// Static keys followed by loaded writable then loaded readonly addresses.
    pub keys: Vec<Pubkey>,
    pub num_signers: usize,
    pub top: Vec<Ix>,
    /// Inner instructions grouped by top-level index (outer index, flattened list).
    pub inner: Vec<(usize, Vec<Ix>)>,
    pub pre_balances: Vec<u64>,
    pub post_balances: Vec<u64>,
    pub pre_tokens: Vec<TokenBal>,
    pub post_tokens: Vec<TokenBal>,
    pub logs: Vec<String>,
    /// False for pre-execution (deshred) transactions: no meta yet.
    pub has_meta: bool,
}

impl ChainTx {
    pub fn fee_payer(&self) -> Option<&Pubkey> {
        self.keys.first()
    }

    pub fn signers(&self) -> &[Pubkey] {
        &self.keys[..self.num_signers.min(self.keys.len())]
    }

    /// All instructions, top-level and inner, in execution order.
    pub fn all_ixs(&self) -> impl Iterator<Item = &Ix> {
        let inner: HashMap<usize, &Vec<Ix>> = self.inner.iter().map(|(i, v)| (*i, v)).collect();
        let mut out: Vec<&Ix> = Vec::with_capacity(self.top.len() * 4);
        for (i, ix) in self.top.iter().enumerate() {
            out.push(ix);
            if let Some(v) = inner.get(&i) {
                out.extend(v.iter());
            }
        }
        out.into_iter()
    }

    pub fn invokes(&self, program: &Pubkey) -> bool {
        self.all_ixs().any(|ix| ix.program == *program)
    }

    pub fn key_index(&self, pk: &Pubkey) -> Option<usize> {
        self.keys.iter().position(|k| k == pk)
    }

    pub fn now_ns() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    // ------------------------------------------------------------ from Geyser

    pub fn from_geyser(
        info: &yellowstone_grpc_proto::prelude::SubscribeUpdateTransactionInfo,
        slot: u64,
        observed_at_ns: u64,
    ) -> Option<Self> {
        let tx = info.transaction.as_ref()?;
        let msg = tx.message.as_ref()?;
        let meta = info.meta.as_ref();
        let mut keys: Vec<Pubkey> = msg.account_keys.iter().filter_map(|k| to_pk(k)).collect();
        if let Some(m) = meta {
            keys.extend(m.loaded_writable_addresses.iter().filter_map(|k| to_pk(k)));
            keys.extend(m.loaded_readonly_addresses.iter().filter_map(|k| to_pk(k)));
        }
        let ix = |prog: u32, accs: &[u8], data: &[u8]| -> Option<Ix> {
            Some(Ix {
                program: *keys.get(prog as usize)?,
                accounts: accs
                    .iter()
                    .filter_map(|a| keys.get(*a as usize).copied())
                    .collect(),
                data: data.to_vec(),
            })
        };
        let top = msg
            .instructions
            .iter()
            .filter_map(|i| ix(i.program_id_index, &i.accounts, &i.data))
            .collect();
        let (inner, pre_tokens, post_tokens, logs, fee, failed, pre_b, post_b) = match meta {
            Some(m) => (
                m.inner_instructions
                    .iter()
                    .map(|g| {
                        (
                            g.index as usize,
                            g.instructions
                                .iter()
                                .filter_map(|i| ix(i.program_id_index, &i.accounts, &i.data))
                                .collect(),
                        )
                    })
                    .collect(),
                token_bals_geyser(&m.pre_token_balances, &keys),
                token_bals_geyser(&m.post_token_balances, &keys),
                m.log_messages.clone(),
                m.fee,
                m.err.is_some(),
                m.pre_balances.clone(),
                m.post_balances.clone(),
            ),
            None => Default::default(),
        };
        Some(Self {
            signature: bs58::encode(&info.signature).into_string(),
            slot,
            tx_index: Some(info.index as u32),
            block_time_ms: None,
            observed_at_ns,
            fetched_at_ns: 0,
            fetch_tries: 0,
            source: FeedSource::Geyser,
            failed,
            fee,
            num_signers: msg
                .header
                .as_ref()
                .map(|h| h.num_required_signatures as usize)
                .unwrap_or(1),
            keys,
            top,
            inner,
            pre_balances: pre_b,
            post_balances: post_b,
            pre_tokens,
            post_tokens,
            logs,
            has_meta: meta.is_some(),
        })
    }

    /// Pre-execution transaction reconstructed from shreds (no meta).
    pub fn from_deshred(
        info: &yellowstone_grpc_proto::prelude::SubscribeUpdateDeshredTransactionInfo,
        slot: u64,
        observed_at_ns: u64,
    ) -> Option<Self> {
        let tx = info.transaction.as_ref()?;
        let msg = tx.message.as_ref()?;
        let mut keys: Vec<Pubkey> = msg.account_keys.iter().filter_map(|k| to_pk(k)).collect();
        keys.extend(
            info.loaded_writable_addresses
                .iter()
                .filter_map(|k| to_pk(k)),
        );
        keys.extend(
            info.loaded_readonly_addresses
                .iter()
                .filter_map(|k| to_pk(k)),
        );
        let top = msg
            .instructions
            .iter()
            .filter_map(|i| {
                Some(Ix {
                    program: *keys.get(i.program_id_index as usize)?,
                    accounts: i
                        .accounts
                        .iter()
                        .filter_map(|a| keys.get(*a as usize).copied())
                        .collect(),
                    data: i.data.clone(),
                })
            })
            .collect();
        Some(Self {
            signature: bs58::encode(&info.signature).into_string(),
            slot,
            tx_index: None,
            block_time_ms: None,
            observed_at_ns,
            fetched_at_ns: 0,
            fetch_tries: 0,
            source: FeedSource::Shred,
            failed: false,
            fee: 0,
            num_signers: msg
                .header
                .as_ref()
                .map(|h| h.num_required_signatures as usize)
                .unwrap_or(1),
            keys,
            top,
            inner: vec![],
            pre_balances: vec![],
            post_balances: vec![],
            pre_tokens: vec![],
            post_tokens: vec![],
            logs: vec![],
            has_meta: false,
        })
    }

    // ------------------------------------------------------------ from RPC JSON

    pub fn from_rpc_json(v: &Value) -> anyhow::Result<Self> {
        let tx = &v["transaction"];
        let msg = &tx["message"];
        let meta = &v["meta"];
        let mut keys: Vec<Pubkey> = Vec::new();
        for k in msg["accountKeys"].as_array().into_iter().flatten() {
            // json encoding: strings; jsonParsed: {pubkey: ..}
            let s = k
                .as_str()
                .or_else(|| k["pubkey"].as_str())
                .unwrap_or_default();
            keys.push(s.parse()?);
        }
        for part in ["writable", "readonly"] {
            for k in meta["loadedAddresses"][part]
                .as_array()
                .into_iter()
                .flatten()
            {
                keys.push(k.as_str().unwrap_or_default().parse()?);
            }
        }
        let ix = |i: &Value| -> Option<Ix> {
            Some(Ix {
                program: *keys.get(i["programIdIndex"].as_u64()? as usize)?,
                accounts: i["accounts"]
                    .as_array()?
                    .iter()
                    .filter_map(|a| keys.get(a.as_u64()? as usize).copied())
                    .collect(),
                data: bs58::decode(i["data"].as_str()?).into_vec().ok()?,
            })
        };
        let top = msg["instructions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(ix)
            .collect();
        let inner = meta["innerInstructions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|g| {
                (
                    g["index"].as_u64().unwrap_or(0) as usize,
                    g["instructions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(ix)
                        .collect(),
                )
            })
            .collect();
        let u64s = |x: &Value| {
            x.as_array()
                .map(|a| a.iter().filter_map(|n| n.as_u64()).collect())
                .unwrap_or_default()
        };
        Ok(Self {
            signature: tx["signatures"][0].as_str().unwrap_or_default().to_string(),
            slot: v["slot"].as_u64().unwrap_or(0),
            tx_index: v["transactionIndex"].as_u64().map(|i| i as u32),
            block_time_ms: v["blockTime"].as_i64().map(|t| t * 1000),
            observed_at_ns: 0,
            fetched_at_ns: 0,
            fetch_tries: 0,
            source: FeedSource::Rpc,
            failed: !meta["err"].is_null(),
            fee: meta["fee"].as_u64().unwrap_or(0),
            num_signers: msg["header"]["numRequiredSignatures"].as_u64().unwrap_or(1) as usize,
            pre_tokens: token_bals_json(&meta["preTokenBalances"], &keys),
            post_tokens: token_bals_json(&meta["postTokenBalances"], &keys),
            pre_balances: u64s(&meta["preBalances"]),
            post_balances: u64s(&meta["postBalances"]),
            logs: meta["logMessages"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            keys,
            top,
            inner,
            has_meta: true,
        })
    }
}

fn to_pk(b: &[u8]) -> Option<Pubkey> {
    Some(Pubkey::new_from_array(b.try_into().ok()?))
}

fn token_bals_geyser(
    v: &[yellowstone_grpc_proto::prelude::TokenBalance],
    keys: &[Pubkey],
) -> Vec<TokenBal> {
    v.iter()
        .filter_map(|b| {
            let amt = b.ui_token_amount.as_ref()?;
            Some(TokenBal {
                account: *keys.get(b.account_index as usize)?,
                mint: b.mint.parse().ok()?,
                owner: b.owner.parse().unwrap_or_default(),
                program: b.program_id.parse().unwrap_or_default(),
                amount: amt.amount.parse().unwrap_or(0),
                decimals: amt.decimals as u8,
            })
        })
        .collect()
}

fn token_bals_json(v: &Value, keys: &[Pubkey]) -> Vec<TokenBal> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| {
            Some(TokenBal {
                account: *keys.get(b["accountIndex"].as_u64()? as usize)?,
                mint: b["mint"].as_str()?.parse().ok()?,
                owner: b["owner"]
                    .as_str()
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or_default(),
                program: b["programId"]
                    .as_str()
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or_default(),
                amount: b["uiTokenAmount"]["amount"].as_str()?.parse().ok()?,
                decimals: b["uiTokenAmount"]["decimals"].as_u64().unwrap_or(0) as u8,
            })
        })
        .collect()
}
