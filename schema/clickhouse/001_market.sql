-- Market firehose: every decoded swap / pool update on every tracked token,
-- not only our trades. This is what makes counterfactual exit replay and
-- eventually leader-free trading possible. Append-only, ClickHouse.

CREATE DATABASE IF NOT EXISTS mkt;

-- Every swap on a token we track (leader-touched tokens + discovery universe).
CREATE TABLE IF NOT EXISTS mkt.swaps
(
    slot              UInt64,
    tx_index          UInt32,
    ix_index          UInt16,
    block_time        DateTime64(3, 'UTC'),
    observed_at       DateTime64(6, 'UTC'),          -- local receive time
    source            LowCardinality(String),         -- shred | geyser | rpc
    signature         String,
    signer            String,
    fee_payer         String,
    mint              LowCardinality(String),
    pool              LowCardinality(String),
    venue             LowCardinality(String),          -- pump_fun_curve | pump_swap | raydium_launch_lab | meteora_dbc | ...
    side              Enum8('buy' = 1, 'sell' = 2),
    sol_amount        UInt64,                          -- lamports
    token_amount      UInt64,                          -- base units
    price_sol         Float64,
    pool_sol_after    Nullable(UInt64),
    pool_token_after  Nullable(UInt64),
    curve_progress    Nullable(Float32),               -- bonding curve % toward migration
    priority_fee      UInt64,                          -- lamports, from compute budget ix
    jito_tip          UInt64,                          -- lamports, transfers to tip accounts
    cu_consumed       UInt32,
    via_aggregator    LowCardinality(String),          -- '' | jupiter | okx | ...
    is_tracked_leader Bool,
    wallet_tags       Array(LowCardinality(String))    -- sniper, bundler, kol, smart, fresh, dev, insider...
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(block_time)
ORDER BY (mint, slot, tx_index, ix_index)
TTL toDateTime(block_time) + INTERVAL 400 DAY;

-- Token lifecycle events: create, migrate, authority changes, metadata updates.
CREATE TABLE IF NOT EXISTS mkt.token_events
(
    slot        UInt64,
    block_time  DateTime64(3, 'UTC'),
    signature   String,
    mint        LowCardinality(String),
    kind        LowCardinality(String),  -- create | migrate | mint_auth_revoked | freeze_auth_revoked | lp_burn | metadata_update | creator_fee_claim | dev_transfer
    actor       String,
    payload     String                    -- JSON
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(block_time)
ORDER BY (mint, slot);

-- Rolling per-token feature snapshots (the "GMGN / Axiom panel" over time).
-- Written by the feature service at fixed cadence and at every decision point.
CREATE TABLE IF NOT EXISTS mkt.token_snapshots
(
    ts                     DateTime64(3, 'UTC'),
    mint                   LowCardinality(String),
    trigger                LowCardinality(String),   -- cadence | leader_buy | our_entry | our_exit
    age_secs               UInt32,
    venue                  LowCardinality(String),
    launchpad              LowCardinality(String),
    migrated               Bool,
    curve_progress         Nullable(Float32),
    price_sol              Float64,
    mcap_usd               Float64,
    liquidity_sol          Float64,
    sol_usd                Float64,
    -- flow, per window
    buys_5s UInt32, sells_5s UInt32, buys_1m UInt32, sells_1m UInt32, buys_5m UInt32, sells_5m UInt32, buys_1h UInt32, sells_1h UInt32,
    vol_sol_5s Float64, vol_sol_1m Float64, vol_sol_5m Float64, vol_sol_1h Float64,
    net_flow_sol_1m Float64, net_flow_sol_5m Float64,
    tps_1m Float32,                                   -- swaps per second on this token
    unique_traders_5m UInt32, new_wallets_5m UInt32,
    ret_1m Float32, ret_5m Float32, ret_1h Float32,
    realized_vol_5m Float32,
    ath_price_sol Float64, drawdown_from_ath Float32,
    -- holders / distribution
    holders UInt32,
    top10_pct Float32,
    dev_holding_pct Float32,
    sniper_pct Float32,
    bundler_pct Float32,
    insider_pct Float32,
    fresh_wallet_pct Float32,
    smart_money_holders UInt16,
    kol_holders UInt16,
    tracked_leader_holders UInt16,
    -- dev / creator history
    dev_prior_launches UInt32,
    dev_prior_migrations UInt32,
    dev_prior_rug_rate Float32,
    dev_sold Bool,
    -- safety
    mint_authority_revoked Bool,
    freeze_authority_revoked Bool,
    lp_burned_pct Nullable(Float32),
    token_2022_extensions Array(LowCardinality(String)),
    creator_fee_mode LowCardinality(String),          -- creator | cashback | none
    -- social / attention
    has_twitter Bool, has_telegram Bool, has_website Bool,
    twitter_handle_reused Bool,                        -- handle seen on earlier mints
    twitter_followers Nullable(UInt32),
    mentions_5m UInt32, mentions_1h UInt32,
    kol_mentions_1h UInt16,
    dex_paid Bool, dex_boosts UInt16,
    -- market regime context
    sol_ret_1h Float32,
    launches_1h UInt32, migrations_1h UInt32,
    meme_volume_1h_sol Float64
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(ts)
ORDER BY (mint, ts);
