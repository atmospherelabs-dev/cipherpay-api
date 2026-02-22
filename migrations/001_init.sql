-- CipherPay Database Schema (SQLite)

CREATE TABLE IF NOT EXISTS merchants (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    api_key_hash TEXT NOT NULL UNIQUE,
    dashboard_token_hash TEXT NOT NULL DEFAULT '',
    ufvk TEXT NOT NULL UNIQUE,
    payment_address TEXT NOT NULL DEFAULT '',
    webhook_url TEXT,
    webhook_secret TEXT NOT NULL DEFAULT '',
    recovery_email TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    merchant_id TEXT NOT NULL REFERENCES merchants(id),
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS invoices (
    id TEXT PRIMARY KEY,
    merchant_id TEXT NOT NULL REFERENCES merchants(id),
    memo_code TEXT NOT NULL UNIQUE,
    product_id TEXT REFERENCES products(id),
    product_name TEXT,
    size TEXT,
    price_eur REAL NOT NULL,
    price_zec REAL NOT NULL,
    zec_rate_at_creation REAL NOT NULL,
    payment_address TEXT NOT NULL DEFAULT '',
    zcash_uri TEXT NOT NULL DEFAULT '',
    shipping_alias TEXT,
    shipping_address TEXT,
    shipping_region TEXT,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'detected', 'confirmed', 'expired', 'shipped', 'refunded')),
    detected_txid TEXT,
    detected_at TEXT,
    confirmed_at TEXT,
    shipped_at TEXT,
    expires_at TEXT NOT NULL,
    purge_after TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_invoices_status ON invoices(status);
CREATE INDEX IF NOT EXISTS idx_invoices_memo ON invoices(memo_code);

CREATE TABLE IF NOT EXISTS products (
    id TEXT PRIMARY KEY,
    merchant_id TEXT NOT NULL REFERENCES merchants(id),
    slug TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    price_eur REAL NOT NULL,
    variants TEXT,
    active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(merchant_id, slug)
);

CREATE INDEX IF NOT EXISTS idx_products_merchant ON products(merchant_id);

CREATE TABLE IF NOT EXISTS webhook_deliveries (
    id TEXT PRIMARY KEY,
    invoice_id TEXT NOT NULL REFERENCES invoices(id),
    url TEXT NOT NULL,
    payload TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'delivered', 'failed')),
    attempts INTEGER NOT NULL DEFAULT 0,
    last_attempt_at TEXT,
    next_retry_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
