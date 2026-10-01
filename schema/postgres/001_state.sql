-- Operational state + the trade journal. Postgres.
-- The journal is the "many dimensions" log: each decision references the
-- ClickHouse snapshot taken at that instant (mkt.token_snapshots, trigger=...).

CREATE TABLE leaders (
    address        TEXT PRIMARY KEY,
    label          TEXT NOT NULL DEFAULT '',
    status         TEXT NOT NULL CHECK (status IN ('candidate','probation','active','paused','retired')),
    added_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    retired_reason TEXT,
    style          JSONB NOT NULL DEFAULT '{}'      -- median hold, venues, mcap band, hours active...
);

-- Periodic re-scoring output (engine-core wallet_score::WalletReport).
CREATE TABLE leader_scores (
    address           TEXT REFERENCES leaders(address),
    scored_at         TIMESTAMPTZ NOT NULL,
    window_days       INT NOT NULL,
    n                 INT, win_rate REAL, median_ret REAL, median_hold_secs REAL,
    profit_factor     REAL, sniper_share REAL, active_hours_per_day REAL,
    copy_n            INT, copy_median_ret REAL, imitation_penalty REAL,
    live_copy_median_ret REAL,                      -- realised by us, once we have copies
    score             REAL,
    rejects           TEXT[],
    PRIMARY KEY (address, scored_at)
);

CREATE TABLE trading_wallets (
    pubkey          TEXT PRIMARY KEY,
    role            TEXT NOT NULL CHECK (role IN ('hot','treasury','fee_payer','burner')),
    custody         TEXT NOT NULL CHECK (custody IN ('local_keystore','turnkey','privy','hardware')),
    status          TEXT NOT NULL CHECK (status IN ('active','draining','retired')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    retired_at      TIMESTAMPTZ,
    max_balance_sol NUMERIC NOT NULL                -- auto-sweep above this
);

CREATE TABLE wallet_transfers (
    id          BIGSERIAL PRIMARY KEY,
    ts          TIMESTAMPTZ NOT NULL,
    from_pubkey TEXT NOT NULL,
    to_pubkey   TEXT NOT NULL,
    lamports    BIGINT NOT NULL,
    reason      TEXT NOT NULL,                      -- fund | sweep | rotate | cex_withdraw | cex_deposit
    signature   TEXT
);

-- Every leader buy we saw, whether or not we copied it. Skips are data.
CREATE TABLE copy_signals (
    id                 BIGSERIAL PRIMARY KEY,
    leader             TEXT NOT NULL,
    leader_signature   TEXT NOT NULL UNIQUE,
    leader_slot        BIGINT NOT NULL,
    leader_tx_index    INT,
    mint               TEXT NOT NULL,
    venue              TEXT NOT NULL,
    side               TEXT NOT NULL CHECK (side IN ('buy','sell')),
    leader_sol         BIGINT NOT NULL,
    leader_price       DOUBLE PRECISION NOT NULL,
    detected_at        TIMESTAMPTZ NOT NULL,
    detect_source      TEXT NOT NULL,               -- shred | geyser | rpc
    detect_latency_ms  REAL,                        -- block time → our receive
    detect_slot_lag    INT,
    decision           TEXT NOT NULL,               -- buy | skip
    skip_reason        TEXT,
    size_lamports      BIGINT,
    capped_by          TEXT,
    model_version      TEXT,                        -- entry model / rules version
    model_score        REAL,
    snapshot_ts        TIMESTAMPTZ                  -- key into mkt.token_snapshots
);

CREATE TABLE positions (
    id                BIGSERIAL PRIMARY KEY,
    mint              TEXT NOT NULL,
    leader            TEXT NOT NULL,
    wallet            TEXT NOT NULL REFERENCES trading_wallets(pubkey),
    mode              TEXT NOT NULL CHECK (mode IN ('shadow','paper','live')),
    exit_policy       TEXT NOT NULL,
    exit_policy_hash  TEXT NOT NULL,                -- hash of the exact parameters
    opened_at         TIMESTAMPTZ NOT NULL,
    closed_at         TIMESTAMPTZ,
    entry_price       DOUBLE PRECISION NOT NULL,
    entry_pool_sol    DOUBLE PRECISION,
    cost_lamports     BIGINT NOT NULL,
    proceeds_lamports BIGINT NOT NULL DEFAULT 0,
    fees_lamports     BIGINT NOT NULL DEFAULT 0,    -- priority + tips + venue fees
    peak_multiple     REAL,                         -- MFE
    trough_multiple   REAL,                         -- MAE
    leader_exit_at    TIMESTAMPTZ,
    leader_exit_multiple REAL,                      -- what mirroring would have got
    realized_pnl_sol  DOUBLE PRECISION,
    UNIQUE (mint, wallet, opened_at)
);

-- Every transaction we send (buys, sells, retries). Execution quality lives here.
CREATE TABLE orders (
    id                 BIGSERIAL PRIMARY KEY,
    position_id        BIGINT REFERENCES positions(id),
    signal_id          BIGINT REFERENCES copy_signals(id),
    side               TEXT NOT NULL CHECK (side IN ('buy','sell')),
    reason             TEXT NOT NULL,               -- copy | add | take_profit:0 | trailing | stop_loss | leader_sell | ...
    created_at         TIMESTAMPTZ NOT NULL,
    signature          TEXT,
    senders            TEXT[] NOT NULL,             -- jito, helius_sender, nozomi, 0slot, astralane, rpc
    landed_by          TEXT,
    priority_fee       BIGINT, tip_lamports BIGINT, cu_limit INT, cu_used INT,
    slippage_bps_limit INT,
    expected_price     DOUBLE PRECISION,
    fill_price         DOUBLE PRECISION,
    slippage_bps_real  REAL,
    sent_slot          BIGINT,
    landed_slot        BIGINT,
    slots_after_leader INT,                         -- landed_slot - leader_slot
    txs_between_leader INT,                         -- pool txs between leader and us (imitation penalty)
    status             TEXT NOT NULL CHECK (status IN ('pending','landed','failed','expired','dropped')),
    error              TEXT
);

-- Counterfactual results of shadow exit policies on every position.
CREATE TABLE shadow_exits (
    position_id  BIGINT REFERENCES positions(id),
    exit_policy  TEXT NOT NULL,
    policy_hash  TEXT NOT NULL,
    pnl_sol      DOUBLE PRECISION NOT NULL,
    ret          REAL NOT NULL,
    held_ms      BIGINT NOT NULL,
    fully_closed BOOLEAN NOT NULL,
    exits        JSONB NOT NULL,                    -- [ExitAction]
    computed_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (position_id, policy_hash)
);

CREATE TABLE risk_events (
    id      BIGSERIAL PRIMARY KEY,
    ts      TIMESTAMPTZ NOT NULL DEFAULT now(),
    kind    TEXT NOT NULL,                          -- kill_switch | daily_loss | feed_stale | sender_degraded | balance_low
    detail  JSONB NOT NULL DEFAULT '{}'
);
