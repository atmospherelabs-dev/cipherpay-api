# CipherPay

**Shielded Zcash payment gateway.** Accept private ZEC payments on any website — like Stripe, but for Zcash.

Built on [CipherScan](https://cipherscan.app) APIs. No full node required.

## Features

- **Shielded payments** — Orchard + Sapling trial decryption
- **Mempool detection** — payments detected in ~5 seconds, confirmed after 1 block
- **REST API** — create invoices, manage products, stream payment status via SSE
- **Merchant dashboard** — register, manage products, configure webhooks
- **HMAC-signed webhooks** — `invoice.confirmed`, `invoice.expired`, `invoice.cancelled`
- **Auto-purge** — customer shipping data purged after configurable period (default 30 days)
- **Self-hostable** — single binary, SQLite, no external dependencies beyond CipherScan

## Architecture

```
┌──────────────┐         ┌──────────────┐         ┌──────────────┐
│  Your Store  │──API───▶│  CipherPay   │──API───▶│  CipherScan  │
│  (frontend)  │         │   (this)     │◀──poll──│  (blockchain) │
└──────────────┘         └──────────────┘         └──────────────┘
                               │                        ▲
                         ┌─────┴─────┐                  │
                         │  Scanner  │──mempool/blocks───┘
                         └───────────┘
```

## Quick Start

```bash
# Clone and configure
cp .env.example .env
# Set ENCRYPTION_KEY: openssl rand -hex 32

# Run
cargo run
```

The server starts on `http://localhost:3080`.

## API Overview

### Merchant Registration

```bash
curl -X POST http://localhost:3080/api/merchants \
  -H "Content-Type: application/json" \
  -d '{"ufvk": "uview1...", "name": "My Store"}'
```

Returns `api_key` and `dashboard_token` — save these, they're shown only once.

### Create Invoice

```bash
curl -X POST http://localhost:3080/api/invoices \
  -H "Authorization: Bearer <api_key>" \
  -H "Content-Type: application/json" \
  -d '{
    "product_name": "T-Shirt",
    "size": "L",
    "price_eur": 65.00
  }'
```

### Payment Status (SSE)

```bash
curl -N http://localhost:3080/api/invoices/<id>/stream
```

### Webhooks

Configure your webhook URL in the dashboard. CipherPay sends POST requests signed with HMAC-SHA256:

| Event | When |
|-------|------|
| `invoice.confirmed` | Payment confirmed (1 block) |
| `invoice.expired` | Invoice timed out |
| `invoice.cancelled` | Invoice cancelled |

Headers: `X-CipherPay-Signature`, `X-CipherPay-Timestamp`

Signature = HMAC-SHA256(`timestamp.body`, `webhook_secret`)

## Project Structure

```
src/
├── main.rs                 # Server setup, scanner spawn
├── config.rs               # Environment configuration
├── db.rs                   # SQLite pool + migrations
├── email.rs                # SMTP recovery emails
├── api/
│   ├── mod.rs              # Route config, checkout, SSE
│   ├── auth.rs             # Sessions, recovery
│   ├── invoices.rs         # Invoice CRUD
│   ├── merchants.rs        # Merchant registration
│   ├── products.rs         # Product management
│   └── rates.rs            # ZEC/EUR, ZEC/USD prices
├── invoices/
│   ├── mod.rs              # Invoice logic, expiry, purge
│   ├── matching.rs         # Memo-to-invoice matching
│   └── pricing.rs          # CoinGecko price feed + cache
├── scanner/
│   ├── mod.rs              # Mempool + block polling loop
│   ├── mempool.rs          # Mempool tx fetching
│   ├── blocks.rs           # Block scanning
│   └── decrypt.rs          # Orchard trial decryption
└── webhooks/
    └── mod.rs              # HMAC dispatch + retry
```

## Configuration

See [`.env.example`](.env.example) for all options. Key settings:

| Variable | Description |
|----------|-------------|
| `DATABASE_URL` | SQLite path (default: `sqlite:cipherpay.db`) |
| `CIPHERSCAN_API_URL` | CipherScan API endpoint |
| `NETWORK` | `testnet` or `mainnet` |
| `ENCRYPTION_KEY` | 32-byte hex key for UFVK encryption at rest |
| `MEMPOOL_POLL_INTERVAL_SECS` | How often to scan mempool (default: 5s) |
| `BLOCK_POLL_INTERVAL_SECS` | How often to scan blocks (default: 15s) |
| `INVOICE_EXPIRY_MINUTES` | Invoice TTL (default: 30min) |
| `DATA_PURGE_DAYS` | Days before shipping data is purged (default: 30) |

## Deployment

Recommended: systemd + Caddy on a VPS.

```bash
cargo build --release
# Binary at target/release/cipherpay
```

See the companion frontend at [cipherpay](https://github.com/atmospherelabs-dev/cipherpay-web) for the hosted checkout and merchant dashboard.

## License

MIT
