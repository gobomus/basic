//! Verifies our hand-written builders and decoders against the official Pump
//! IDLs vendored in engine/idl (from github.com/pump-fun/pump-public-docs).
//! If Pump ships an interface change and the IDLs are refreshed, these tests
//! fail before anything is sent on-chain.

use std::collections::HashMap;

use chain::borsh::{anchor_disc, EVENT_IX_TAG};
use chain::consts::*;
use chain::pda;
use chain::pump::{self, CurveCoin};
use chain::pump_amm::{self, AmmCoin};
use serde_json::Value;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;

fn idl(name: &str) -> Value {
    let p = format!("{}/../../idl/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn find<'a>(arr: &'a Value, name: &str) -> &'a Value {
    arr.as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == name)
        .unwrap_or_else(|| panic!("{name} not in IDL"))
}

fn disc_of(v: &Value) -> Vec<u8> {
    v["discriminator"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_u64().unwrap() as u8)
        .collect()
}

#[test]
fn discriminators_match_idl() {
    let p = idl("pump.json");
    let a = idl("pump_amm.json");
    for n in [
        "buy",
        "buy_exact_sol_in",
        "buy_v2",
        "buy_exact_quote_in_v2",
        "sell",
        "sell_v2",
    ] {
        assert_eq!(
            pump::ix_disc(n).to_vec(),
            disc_of(find(&p["instructions"], n)),
            "pump ix {n}"
        );
    }
    for n in ["buy", "sell", "buy_exact_quote_in"] {
        assert_eq!(
            pump_amm::ix_disc(n).to_vec(),
            disc_of(find(&a["instructions"], n)),
            "amm ix {n}"
        );
    }
    for n in ["TradeEvent", "CreateEvent", "CompleteEvent"] {
        assert_eq!(
            pump::event_disc(n).to_vec(),
            disc_of(find(&p["events"], n)),
            "pump event {n}"
        );
    }
    for n in ["BuyEvent", "SellEvent"] {
        assert_eq!(
            pump_amm::event_disc(n).to_vec(),
            disc_of(find(&a["events"], n)),
            "amm event {n}"
        );
    }
    for (idl_v, n) in [(&p, "BondingCurve"), (&a, "Pool"), (&a, "GlobalConfig")] {
        assert_eq!(
            anchor_disc("account", n).to_vec(),
            disc_of(find(&idl_v["accounts"], n)),
            "account {n}"
        );
    }
}

/// Resolve every IDL account (fixed address or PDA recipe) from the accounts
/// we actually built, and compare address + writable + signer flags.
fn check_ix_against_idl(
    ix: &Instruction,
    idl_v: &Value,
    name: &str,
    program: &Pubkey,
    extra: &HashMap<&str, Pubkey>,
) {
    let def = find(&idl_v["instructions"], name);
    let accs = def["accounts"].as_array().unwrap();
    assert!(
        ix.accounts.len() >= accs.len(),
        "{name}: built {} accounts, IDL has {}",
        ix.accounts.len(),
        accs.len()
    );
    let mut by_name: HashMap<String, Pubkey> = HashMap::new();
    for (i, a) in accs.iter().enumerate() {
        by_name.insert(
            a["name"].as_str().unwrap().to_string(),
            ix.accounts[i].pubkey,
        );
    }
    for (i, a) in accs.iter().enumerate() {
        let n = a["name"].as_str().unwrap();
        let meta = &ix.accounts[i];
        if a["optional"].as_bool() == Some(true) && meta.pubkey == *program {
            continue; // Anchor: absent optional account is passed as the program id
        }
        assert_eq!(
            meta.is_writable,
            a["writable"].as_bool().unwrap_or(false),
            "{name}.{n} writable"
        );
        assert_eq!(
            meta.is_signer,
            a["signer"].as_bool().unwrap_or(false),
            "{name}.{n} signer"
        );
        if let Some(addr) = a["address"].as_str() {
            assert_eq!(meta.pubkey.to_string(), addr, "{name}.{n} address");
        }
        if let Some(pda_def) = a.get("pda") {
            let mut seeds: Vec<Vec<u8>> = vec![];
            for s in pda_def["seeds"].as_array().unwrap() {
                match s["kind"].as_str().unwrap() {
                    "const" => seeds.push(
                        s["value"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|x| x.as_u64().unwrap() as u8)
                            .collect(),
                    ),
                    "account" => {
                        let path = s["path"].as_str().unwrap();
                        let pk = extra
                            .get(path)
                            .copied()
                            .or_else(|| by_name.get(path).copied())
                            .unwrap_or_else(|| panic!("{name}.{n}: unresolved seed path {path}"));
                        seeds.push(pk.to_bytes().to_vec());
                    }
                    k => panic!("unsupported seed kind {k}"),
                }
            }
            let prog = match pda_def.get("program") {
                None => *program,
                Some(p) if p["kind"] == "const" => Pubkey::new_from_array(
                    p["value"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|x| x.as_u64().unwrap() as u8)
                        .collect::<Vec<_>>()
                        .try_into()
                        .unwrap(),
                ),
                Some(p) => by_name[p["path"].as_str().unwrap()],
            };
            let refs: Vec<&[u8]> = seeds.iter().map(|s| s.as_slice()).collect();
            let expect = Pubkey::find_program_address(&refs, &prog).0;
            assert_eq!(meta.pubkey, expect, "{name}.{n} PDA");
        }
    }
    assert_eq!(ix.program_id, *program);
    assert_eq!(
        &ix.data[..8],
        &disc_of(def)[..],
        "{name} discriminator in data"
    );
}

#[test]
fn pump_curve_builders_match_idl() {
    let p = idl("pump.json");
    let mint = Pubkey::new_unique();
    let creator = Pubkey::new_unique();
    let user = Pubkey::new_unique();
    for (tp, mayhem) in [(TOKEN_PROGRAM, false), (TOKEN_2022_PROGRAM, true)] {
        let coin = CurveCoin::sol_paired(mint, creator, tp, mayhem);
        let extra = HashMap::from([("bonding_curve.creator", creator)]);
        for salt in 0..64 {
            let b = pump::buy_exact_quote_in_v2(&coin, &user, 1_000_000_000, 1, salt);
            check_ix_against_idl(&b, &p, "buy_exact_quote_in_v2", &PUMP_PROGRAM, &extra);
            assert_eq!(b.accounts.len(), 27);
            let fee = b.accounts[6].pubkey;
            if mayhem {
                assert!(PUMP_RESERVED_FEE_RECIPIENTS.contains(&fee));
            } else {
                assert!(PUMP_FEE_RECIPIENTS.contains(&fee));
            }
            assert!(PUMP_BUYBACK_FEE_RECIPIENTS.contains(&b.accounts[8].pubkey));
            let s = pump::sell_v2(&coin, &user, 5, 1, salt);
            check_ix_against_idl(&s, &p, "sell_v2", &PUMP_PROGRAM, &extra);
            assert_eq!(s.accounts.len(), 26);
        }
        // user token accounts use the coin's token program
        let b = pump::buy_exact_quote_in_v2(&coin, &user, 1, 1, 0);
        assert_eq!(b.accounts[14].pubkey, pda::ata(&user, &mint, &tp));
        assert_eq!(
            b.accounts[15].pubkey,
            pda::ata(&user, &WSOL_MINT, &TOKEN_PROGRAM)
        );
        assert_eq!(&b.data[8..16], &1u64.to_le_bytes());
    }
}

#[test]
fn pump_amm_builders_match_idl_and_sdk_remaining_accounts() {
    let a = idl("pump_amm.json");
    let user = Pubkey::new_unique();
    let base_mint = Pubkey::new_unique();
    for (cashback, has_creator) in [(false, true), (true, true), (false, false), (true, false)] {
        let coin_creator = if has_creator {
            Pubkey::new_unique()
        } else {
            Pubkey::default()
        };
        let coin = AmmCoin {
            pool: Pubkey::new_unique(),
            base_mint,
            quote_mint: WSOL_MINT,
            pool_base_token_account: Pubkey::new_unique(),
            pool_quote_token_account: Pubkey::new_unique(),
            base_token_program: TOKEN_2022_PROGRAM,
            quote_token_program: TOKEN_PROGRAM,
            coin_creator,
            is_mayhem_mode: false,
            is_cashback_coin: cashback,
            protocol_fee_recipient: Pubkey::new_unique(),
            buyback_fee_recipient: PUMP_BUYBACK_FEE_RECIPIENTS[0],
        };
        let extra = HashMap::from([("pool.coin_creator", coin_creator)]);

        let buys = pump_amm::buy_instructions(&coin, &user, 1000, 2000);
        let swap = buys
            .iter()
            .find(|i| i.program_id == PUMP_AMM_PROGRAM)
            .unwrap();
        check_ix_against_idl(swap, &a, "buy", &PUMP_AMM_PROGRAM, &extra);
        let rem = &swap.accounts[23..];
        let uva = pda::amm_user_volume_accumulator(&user);
        let mut expect = vec![];
        if cashback {
            expect.push((pda::ata(&uva, &WSOL_MINT, &TOKEN_PROGRAM), true));
        }
        if has_creator {
            expect.push((pda::amm_pool_v2(&base_mint), false));
        }
        expect.push((coin.buyback_fee_recipient, false));
        expect.push((
            pda::ata(&coin.buyback_fee_recipient, &WSOL_MINT, &TOKEN_PROGRAM),
            true,
        ));
        assert_eq!(
            rem.iter()
                .map(|m| (m.pubkey, m.is_writable))
                .collect::<Vec<_>>(),
            expect,
            "buy remaining (cashback={cashback})"
        );
        assert_eq!(swap.data.len(), 8 + 8 + 8 + 1);
        // WSOL wrap: ata(base), ata(wsol), transfer, sync, buy, close
        assert_eq!(buys.len(), 6);

        let sells = pump_amm::sell_instructions(&coin, &user, 1000, 1);
        let swap = sells
            .iter()
            .find(|i| i.program_id == PUMP_AMM_PROGRAM)
            .unwrap();
        check_ix_against_idl(swap, &a, "sell", &PUMP_AMM_PROGRAM, &extra);
        let rem = &swap.accounts[21..];
        let mut expect = vec![];
        if cashback {
            expect.push((pda::ata(&uva, &WSOL_MINT, &TOKEN_PROGRAM), true));
            expect.push((uva, true));
        }
        if has_creator {
            expect.push((pda::amm_pool_v2(&base_mint), false));
        }
        expect.push((coin.buyback_fee_recipient, false));
        expect.push((
            pda::ata(&coin.buyback_fee_recipient, &WSOL_MINT, &TOKEN_PROGRAM),
            true,
        ));
        assert_eq!(
            rem.iter()
                .map(|m| (m.pubkey, m.is_writable))
                .collect::<Vec<_>>(),
            expect,
            "sell remaining"
        );
        assert_eq!(sells.len(), 3); // ata(wsol), sell, close
    }
}

// ---------------------------------------------------------------- IDL-driven borsh encoder

struct Enc<'a> {
    types: &'a Value,
    seed: u64,
    out: Vec<u8>,
    fields: HashMap<String, Value>,
}

impl<'a> Enc<'a> {
    fn next(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }
    fn value(&mut self, ty: &Value) -> Value {
        if let Some(s) = ty.as_str() {
            return match s {
                "u8" => {
                    let v = (self.next() % 256) as u8;
                    self.out.push(v);
                    Value::from(v)
                }
                "bool" => {
                    let v = self.next() % 2 == 1;
                    self.out.push(v as u8);
                    Value::from(v)
                }
                "u16" => {
                    let v = (self.next() % 65536) as u16;
                    self.out.extend(v.to_le_bytes());
                    Value::from(v)
                }
                "u32" => {
                    let v = self.next() as u32;
                    self.out.extend(v.to_le_bytes());
                    Value::from(v)
                }
                "u64" => {
                    let v = self.next() >> 2;
                    self.out.extend(v.to_le_bytes());
                    Value::from(v)
                }
                "i64" => {
                    let v = (self.next() >> 2) as i64;
                    self.out.extend(v.to_le_bytes());
                    Value::from(v)
                }
                "u128" | "i128" => {
                    let v = (self.next() >> 8) as u128;
                    self.out.extend(v.to_le_bytes());
                    Value::from(v.to_string())
                }
                "pubkey" => {
                    let mut b = [0u8; 32];
                    for c in b.chunks_mut(8) {
                        c.copy_from_slice(&self.next().to_le_bytes());
                    }
                    self.out.extend(b);
                    Value::from(Pubkey::new_from_array(b).to_string())
                }
                "string" => {
                    let s = format!("s{}", self.next() % 1000);
                    self.out.extend((s.len() as u32).to_le_bytes());
                    self.out.extend(s.as_bytes());
                    Value::from(s)
                }
                other => panic!("type {other}"),
            };
        }
        if let Some(v) = ty.get("vec") {
            let n = (self.next() % 3) as usize;
            self.out.extend((n as u32).to_le_bytes());
            return Value::Array((0..n).map(|_| self.value(v)).collect());
        }
        if let Some(arr) = ty.get("array") {
            let n = arr[1].as_u64().unwrap() as usize;
            return Value::Array((0..n).map(|_| self.value(&arr[0])).collect());
        }
        if let Some(d) = ty.get("defined") {
            let name = d["name"].as_str().unwrap();
            let t = &find(self.types, name)["type"];
            let mut obj = serde_json::Map::new();
            for f in t["fields"].as_array().unwrap() {
                if f.is_object() {
                    obj.insert(f["name"].as_str().unwrap().into(), self.value(&f["type"]));
                } else {
                    obj.insert("0".into(), self.value(f));
                }
            }
            return Value::Object(obj);
        }
        panic!("unsupported type {ty}")
    }
    fn encode_struct(types: &'a Value, name: &str, seed: u64) -> (Vec<u8>, HashMap<String, Value>) {
        let mut e = Enc {
            types,
            seed,
            out: vec![],
            fields: HashMap::new(),
        };
        let t = &find(types, name)["type"];
        for f in t["fields"].as_array().unwrap() {
            let v = e.value(&f["type"]);
            e.fields.insert(f["name"].as_str().unwrap().into(), v);
        }
        (e.out, e.fields)
    }
}

fn pk(v: &Value) -> Pubkey {
    v.as_str().unwrap().parse().unwrap()
}

#[test]
fn trade_event_decodes_idl_encoded_bytes() {
    let p = idl("pump.json");
    for seed in 1..200u64 {
        let (bytes, f) = Enc::encode_struct(&p["types"], "TradeEvent", seed * 7919);
        let mut data = EVENT_IX_TAG.to_vec();
        data.extend(pump::event_disc("TradeEvent"));
        data.extend(&bytes);
        let Some(pump::PumpEvent::Trade(e)) = pump::decode_event_ix(&data) else {
            panic!("decode failed")
        };
        assert_eq!(e.mint, pk(&f["mint"]));
        assert_eq!(e.user, pk(&f["user"]));
        assert_eq!(e.creator, pk(&f["creator"]));
        assert_eq!(e.sol_amount, f["sol_amount"].as_u64().unwrap());
        assert_eq!(e.token_amount, f["token_amount"].as_u64().unwrap());
        assert_eq!(e.is_buy, f["is_buy"].as_bool().unwrap());
        assert_eq!(
            e.virtual_sol_reserves,
            f["virtual_sol_reserves"].as_u64().unwrap()
        );
        assert_eq!(
            e.virtual_token_reserves,
            f["virtual_token_reserves"].as_u64().unwrap()
        );
        assert_eq!(
            e.real_sol_reserves,
            f["real_sol_reserves"].as_u64().unwrap()
        );
        assert_eq!(e.fee_basis_points, f["fee_basis_points"].as_u64().unwrap());
        assert_eq!(
            e.creator_fee_basis_points,
            f["creator_fee_basis_points"].as_u64().unwrap()
        );
        assert_eq!(e.ix_name, f["ix_name"].as_str().unwrap());
        assert_eq!(e.mayhem_mode, f["mayhem_mode"].as_bool().unwrap());
        assert_eq!(
            e.buyback_fee_basis_points,
            f["buyback_fee_basis_points"].as_u64().unwrap()
        );
        assert_eq!(e.quote_mint, Some(pk(&f["quote_mint"])));
    }
}

#[test]
fn create_and_complete_events_decode() {
    let p = idl("pump.json");
    let (bytes, f) = Enc::encode_struct(&p["types"], "CreateEvent", 42);
    let mut data = pump::event_disc("CreateEvent").to_vec();
    data.extend(&bytes);
    let Some(pump::PumpEvent::Create(e)) = pump::decode_event(&data) else {
        panic!()
    };
    assert_eq!(e.mint, pk(&f["mint"]));
    assert_eq!(e.creator, pk(&f["creator"]));
    assert_eq!(e.symbol, f["symbol"].as_str().unwrap());
    assert_eq!(e.token_program, Some(pk(&f["token_program"])));
    assert_eq!(e.is_mayhem_mode, f["is_mayhem_mode"].as_bool().unwrap());

    let (bytes, f) = Enc::encode_struct(&p["types"], "CompleteEvent", 43);
    let mut data = pump::event_disc("CompleteEvent").to_vec();
    data.extend(&bytes);
    let Some(pump::PumpEvent::Complete(e)) = pump::decode_event(&data) else {
        panic!()
    };
    assert_eq!(e.mint, pk(&f["mint"]));
}

#[test]
fn amm_events_decode_idl_encoded_bytes() {
    let a = idl("pump_amm.json");
    for (name, is_buy) in [("BuyEvent", true), ("SellEvent", false)] {
        for seed in 1..100u64 {
            let (bytes, f) = Enc::encode_struct(&a["types"], name, seed * 104729);
            let mut data = EVENT_IX_TAG.to_vec();
            data.extend(pump_amm::event_disc(name));
            data.extend(&bytes);
            let e = pump_amm::decode_event_ix(&data).expect("decode");
            assert_eq!(e.is_buy, is_buy);
            assert_eq!(e.pool, pk(&f["pool"]));
            assert_eq!(e.user, pk(&f["user"]));
            assert_eq!(e.coin_creator, pk(&f["coin_creator"]));
            assert_eq!(e.protocol_fee_recipient, pk(&f["protocol_fee_recipient"]));
            assert_eq!(
                e.pool_base_token_reserves,
                f["pool_base_token_reserves"].as_u64().unwrap()
            );
            assert_eq!(
                e.pool_quote_token_reserves,
                f["pool_quote_token_reserves"].as_u64().unwrap()
            );
            let base_field = if is_buy {
                "base_amount_out"
            } else {
                "base_amount_in"
            };
            assert_eq!(e.base_amount, f[base_field].as_u64().unwrap());
            let uq = if is_buy {
                "user_quote_amount_in"
            } else {
                "user_quote_amount_out"
            };
            assert_eq!(e.user_quote_amount, f[uq].as_u64().unwrap());
            assert_eq!(
                e.lp_fee_basis_points,
                f["lp_fee_basis_points"].as_u64().unwrap()
            );
            assert_eq!(
                e.coin_creator_fee_basis_points,
                f["coin_creator_fee_basis_points"].as_u64().unwrap()
            );
            assert_eq!(
                e.buyback_fee_basis_points,
                f["buyback_fee_basis_points"].as_u64().unwrap()
            );
            assert_eq!(
                e.virtual_quote_reserves.to_string(),
                f["virtual_quote_reserves"].as_str().unwrap()
            );
        }
    }
}

#[test]
fn accounts_decode_idl_encoded_bytes() {
    let p = idl("pump.json");
    let (bytes, f) = Enc::encode_struct(&p["types"], "BondingCurve", 7);
    let mut data = anchor_disc("account", "BondingCurve").to_vec();
    data.extend(&bytes);
    let bc = pump::BondingCurve::decode(&data).unwrap();
    assert_eq!(bc.creator, pk(&f["creator"]));
    assert_eq!(
        bc.state.virtual_quote_reserves,
        f["virtual_quote_reserves"].as_u64().unwrap()
    );
    assert_eq!(bc.complete, f["complete"].as_bool().unwrap());
    assert_eq!(bc.is_mayhem_mode, f["is_mayhem_mode"].as_bool().unwrap());
    assert_eq!(bc.quote_mint, pk(&f["quote_mint"]));

    let a = idl("pump_amm.json");
    let (bytes, f) = Enc::encode_struct(&a["types"], "Pool", 9);
    let mut data = anchor_disc("account", "Pool").to_vec();
    data.extend(&bytes);
    let pool = pump_amm::Pool::decode(&data).unwrap();
    assert_eq!(pool.base_mint, pk(&f["base_mint"]));
    assert_eq!(
        pool.pool_quote_token_account,
        pk(&f["pool_quote_token_account"])
    );
    assert_eq!(pool.coin_creator, pk(&f["coin_creator"]));
    assert_eq!(
        pool.is_cashback_coin,
        f["is_cashback_coin"].as_bool().unwrap()
    );
    assert_eq!(
        pool.virtual_quote_reserves.to_string(),
        f["virtual_quote_reserves"].as_str().unwrap()
    );

    let (bytes, f) = Enc::encode_struct(&a["types"], "GlobalConfig", 11);
    let mut data = anchor_disc("account", "GlobalConfig").to_vec();
    data.extend(&bytes);
    let g = pump_amm::GlobalConfig::decode(&data).unwrap();
    assert_eq!(
        g.protocol_fee_recipients[7],
        pk(&f["protocol_fee_recipients"][7])
    );
    assert_eq!(g.reserved_fee_recipient, pk(&f["reserved_fee_recipient"]));
    assert_eq!(
        g.reserved_fee_recipients[6],
        pk(&f["reserved_fee_recipients"][6])
    );
    assert_eq!(
        g.buyback_fee_recipients[0],
        pk(&f["buyback_fee_recipients"][0])
    );
    assert_eq!(
        g.buyback_fee_recipients[7],
        pk(&f["buyback_fee_recipients"][7])
    );
}

#[test]
fn quote_math_is_consistent() {
    let s = pump::CurveState {
        virtual_token_reserves: 1_073_000_000_000_000,
        virtual_quote_reserves: 30_000_000_000,
        real_token_reserves: 793_100_000_000_000,
        real_quote_reserves: 0,
    };
    let toks = pump::buy_tokens_for_quote(&s, 1_000_000_000, 125);
    // ~1 SOL into a fresh curve buys roughly 34.5M tokens
    assert!(
        (34_000_000_000_000..35_000_000_000_000).contains(&toks),
        "{toks}"
    );
    let after = pump::CurveState {
        virtual_token_reserves: s.virtual_token_reserves - toks,
        virtual_quote_reserves: s.virtual_quote_reserves + 987_000_000,
        ..s
    };
    let back = pump::sell_quote_for_tokens(&after, toks, 125);
    assert!(back < 1_000_000_000 && back > 950_000_000, "{back}");
    assert_eq!(pump::apply_slippage_down(10_000, 1500), 8_500);

    let out = pump_amm::buy_base_for_quote(1_000_000_000_000, 100_000_000_000, 1_000_000_000, 125);
    assert!(out > 9_700_000_000 && out < 9_900_000_000, "{out}");
    let q = pump_amm::sell_quote_for_base(1_000_000_000_000, 100_000_000_000, out, 125);
    assert!(q < 1_000_000_000 && q > 950_000_000, "{q}");
}

// ---------------------------------------------------------------- Meteora DBC

#[test]
fn meteora_dbc_matches_sdk_idl() {
    use chain::meteora_dbc::{self as dbc, DbcCoin};
    let d = idl("meteora_dbc.json");
    assert_eq!(d["address"], dbc::DBC_PROGRAM.to_string());
    for n in ["swap", "swap2"] {
        assert_eq!(
            dbc::ix_disc(n).to_vec(),
            disc_of(find(&d["instructions"], n)),
            "dbc ix {n}"
        );
    }
    for n in ["EvtSwap", "EvtSwap2"] {
        assert_eq!(
            dbc::event_disc(n).to_vec(),
            disc_of(find(&d["events"], n)),
            "dbc event {n}"
        );
    }
    let user = Pubkey::new_unique();
    for needs_sysvar in [false, true] {
        let coin = DbcCoin {
            pool: Pubkey::new_unique(),
            config: Pubkey::new_unique(),
            base_mint: Pubkey::new_unique(),
            quote_mint: WSOL_MINT,
            base_vault: Pubkey::new_unique(),
            quote_vault: Pubkey::new_unique(),
            base_token_program: TOKEN_2022_PROGRAM,
            quote_token_program: TOKEN_PROGRAM,
            needs_ix_sysvar: needs_sysvar,
        };
        let buys = dbc::buy_instructions(&coin, &user, 1_000_000_000, 5);
        let swap = buys
            .iter()
            .find(|i| i.program_id == dbc::DBC_PROGRAM)
            .unwrap();
        check_ix_against_idl(swap, &d, "swap", &dbc::DBC_PROGRAM, &HashMap::new());
        assert_eq!(swap.accounts.len(), 15 + needs_sysvar as usize);
        // buy: input = user's WSOL ATA, output = user's base ATA
        assert_eq!(
            swap.accounts[3].pubkey,
            pda::ata(&user, &WSOL_MINT, &TOKEN_PROGRAM)
        );
        assert_eq!(
            swap.accounts[4].pubkey,
            pda::ata(&user, &coin.base_mint, &TOKEN_2022_PROGRAM)
        );
        assert_eq!(&swap.data[8..16], &1_000_000_000u64.to_le_bytes());
        assert_eq!(
            swap.accounts[13].pubkey,
            Pubkey::find_program_address(&[b"__event_authority"], &dbc::DBC_PROGRAM).0
        );
        // ata(base), ata(wsol), transfer, sync, swap, close
        assert_eq!(buys.len(), 6);
        let rebuilt = DbcCoin::from_swap_ix(
            &swap.accounts.iter().map(|m| m.pubkey).collect::<Vec<_>>(),
            &swap.data,
        )
        .unwrap();
        assert_eq!(rebuilt, coin, "template round trip");

        let sells = dbc::sell_instructions(&coin, &user, 777, 1);
        let swap = sells
            .iter()
            .find(|i| i.program_id == dbc::DBC_PROGRAM)
            .unwrap();
        check_ix_against_idl(swap, &d, "swap", &dbc::DBC_PROGRAM, &HashMap::new());
        assert_eq!(
            swap.accounts[3].pubkey,
            pda::ata(&user, &coin.base_mint, &TOKEN_2022_PROGRAM)
        );
    }
}

#[test]
fn meteora_dbc_events_decode_idl_encoded_bytes() {
    use chain::meteora_dbc as dbc;
    let d = idl("meteora_dbc.json");
    for (name, v2) in [("EvtSwap", false), ("EvtSwap2", true)] {
        for seed in 1..100u64 {
            let (bytes, f) = Enc::encode_struct(&d["types"], name, seed * 6151);
            let mut data = EVENT_IX_TAG.to_vec();
            data.extend(dbc::event_disc(name));
            data.extend(&bytes);
            let e = dbc::decode_event_ix(&data).expect("decode");
            assert_eq!(e.pool, pk(&f["pool"]));
            assert_eq!(e.config, pk(&f["config"]));
            assert_eq!(e.is_buy, f["trade_direction"].as_u64().unwrap() == 1);
            let r = &f["swap_result"];
            assert_eq!(e.output_amount, r["output_amount"].as_u64().unwrap());
            assert_eq!(
                e.next_sqrt_price.to_string(),
                r["next_sqrt_price"].as_str().unwrap()
            );
            let fees = r["trading_fee"].as_u64().unwrap()
                + r["protocol_fee"].as_u64().unwrap()
                + r["referral_fee"].as_u64().unwrap();
            assert_eq!(e.total_fee, fees);
            if v2 {
                assert_eq!(
                    e.input_amount,
                    r["included_fee_input_amount"].as_u64().unwrap()
                );
                assert_eq!(
                    e.quote_reserve,
                    Some(f["quote_reserve_amount"].as_u64().unwrap())
                );
                assert_eq!(
                    e.migration_threshold,
                    Some(f["migration_threshold"].as_u64().unwrap())
                );
            } else {
                assert_eq!(e.input_amount, r["actual_input_amount"].as_u64().unwrap());
            }
        }
    }
}

#[test]
fn dbc_price_math() {
    use chain::meteora_dbc as dbc;
    // sqrt(P) in Q64.64 for P = 4e-5 lamports per raw token unit
    let p: f64 = 4e-5;
    let sqrt_q64 = (p.sqrt() * 18_446_744_073_709_551_616.0) as u128;
    assert!((dbc::price_raw_from_sqrt(sqrt_q64) - p).abs() / p < 1e-9);
    // 6-decimals token: 4e-5 lamports/unit = 40 lamports/token = 4e-8 SOL/token
    assert!((dbc::price_sol(sqrt_q64, 6) - 4e-8).abs() < 1e-15);
    let out = dbc::estimate_buy(p, 1_000_000_000, 100);
    assert!((out as f64 - 0.99e9 / p).abs() / (out as f64) < 1e-9);
    assert!(dbc::estimate_sell(p, out, 100) < 1_000_000_000);
}
