# CipherPay

**Shielded Zcash payment service with mempool detection.**

Accept shielded ZEC payments on any website. Built by the team behind [CipherScan](https://cipherscan.app).

## What It Does

- Accepts **shielded Zcash payments** (Orchard + Sapling)
- Detects payments in the **mempool** (~5 seconds) before block confirmation
- Provides a **REST API** for creating invoices and checking payment status
- Includes an **embeddable checkout widget** for any website
- **Auto-purges** customer shipping data after 30 days
- Uses **CipherScan APIs** as the blockchain data source -- no node to run

## Quick Start

```bash
cp .env.example .env
docker-compose up -d          # Start PostgreSQL
cargo run                     # Start CipherPay
```

Then register a merchant:

```bash
curl -X POST http://localhost:3080/api/merchants \
  -H "Content-Type: application/json" \
  -d '{"ufvk": "uview1..."}'
```

Create an invoice:

```bash
curl -X POST http://localhost:3080/api/invoices \
  -H "Content-Type: application/json" \
  -d '{"product_name": "[REDACTED] Tee", "size": "L", "price_eur": 65.00}'
```

Embed the checkout widget:

```html
<div id="cipherpay" data-invoice-id="<invoice-id>" data-api="http://localhost:3080"></div>
<script src="http://localhost:3080/widget/cipherpay.js"></script>
```

## Documentation

See [SPEC.md](SPEC.md) for the full technical specification, API reference, and deployment guide.

## Status

**Work in progress.** The trial decryption module (`src/scanner/decrypt.rs`) is a stub that needs full implementation with `zcash_primitives`, `orchard`, and `sapling-crypto` crates. All other components (API, scanner, invoices, webhooks, widget) are functional.

## License

MIT
