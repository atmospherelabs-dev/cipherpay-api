# CipherPay Roadmap

Privacy-preserving Zcash payment gateway. Non-custodial, shielded-only.

---

## Phase 0 -- Testnet MVP (Done)

- [x] Rust/Actix-web service with SQLite
- [x] Merchant registration with UFVK + payment address
- [x] API key authentication (Bearer token, SHA-256 hashed)
- [x] Invoice creation with locked ZEC price at creation time
- [x] Unique memo codes (CP-XXXXXXXX) for payment matching
- [x] QR code generation with zcash: URI (amount + hex-encoded memo)
- [x] Orchard trial decryption ported from zcash-explorer WASM module
- [x] Mempool polling scanner (configurable interval)
- [x] Block scanner for missed/confirmed transactions
- [x] Webhook delivery with retry logic (up to 5 attempts)
- [x] Invoice expiry and automatic status transitions
- [x] Data purge (shipping PII nullified after configurable window)
- [x] CoinGecko price feed with caching and fallback
- [x] Test console UI for end-to-end flow testing
- [x] Testnet guide documentation

## Phase 1 -- Security Hardening (Current)

- [x] Gate simulation endpoints behind `is_testnet()` (prevent free-inventory exploit)
- [x] Payment amount verification with 0.5% slippage tolerance (penny exploit fix)
- [x] Webhook HMAC-SHA256 signing with `X-CipherPay-Signature` + `X-CipherPay-Timestamp`
- [x] Per-merchant `webhook_secret` generated on registration
- [x] Conditional CORS (allow-any on testnet, restricted origins on mainnet)
- [x] `ALLOWED_ORIGINS` config for production deployment
- [x] Concurrent batch raw tx fetching (futures::join_all, batches of 20)
- [x] CipherScan raw tx endpoint (`GET /api/tx/{txid}/raw`)
- [ ] Rate limiting on public endpoints (actix-web-middleware or tower)
- [ ] Invoice lookup auth (merchant can only see own invoices)
- [ ] Merchant registration guard (admin key or invite-only in production)
- [ ] Input validation hardening (UFVK format check, address validation)

## Phase 2 -- Performance & Real-Time

- [ ] **Parallel trial decryption** with `rayon` (.par_iter() over merchants x actions)
- [ ] **CipherScan WebSocket stream** (`ws://api.cipherscan.app/mempool/stream`)
  - Push raw tx hex as Zebra sees new txids
  - Eliminates polling latency entirely
  - Single persistent connection per CipherPay instance
- [ ] **CipherScan batch raw tx endpoint** (`POST /api/tx/raw/batch`)
  - Accept array of txids, return array of hex
  - Single HTTP round-trip instead of N calls
- [ ] Mempool deduplication improvements (bloom filter for seen txids)
- [ ] Sapling trial decryption support (currently Orchard-only)
- [ ] Scanner metrics (Prometheus endpoint: decryption rate, latency, match rate)

## Phase 3 -- Integrations & Go-to-Market

- [ ] **Hosted checkout page** (`pay.cipherpay.app/{invoice_id}`)
  - Standalone payment page merchants can redirect to
  - Mobile-optimized with QR code and deep-link to Zashi/YWallet
- [ ] **Shopify Custom App integration**
  - Merchant installs Custom App in Shopify admin
  - CipherPay marks orders as paid via Shopify Admin REST API
  - (`POST /admin/api/2024-10/orders/{id}/transactions.json`)
  - Avoids Shopify App Store approval process
- [ ] **WooCommerce plugin** (WordPress/PHP webhook receiver)
- [ ] **Embeddable widget** (JS snippet for any website)
  - `<script src="https://pay.cipherpay.app/widget.js">`
  - Drop-in payment button with modal checkout
- [ ] Multi-currency display (EUR, USD, GBP with locked ZEC rate)
- [ ] Email notifications (optional, privacy-conscious: no PII in email body)

## Phase 4 -- Production Infrastructure

- [ ] **Encryption at rest** for UFVKs (AES-256-GCM with HSM-backed key)
- [ ] **Data minimization enforcement**
  - Automatic PII overwrite (shipping data) after 14-day return window
  - Cron job with cryptographic erasure (not just NULL, but overwrite)
  - Zero-knowledge order fulfillment: merchant gets shipping label, not raw address
- [ ] **PostgreSQL migration** for production (multi-tenant, proper indexing)
- [ ] **Multi-node CipherScan infrastructure**
  - Load balancer in front of multiple Zebra nodes
  - Benefits CipherScan explorer, CipherPay, and network decentralization
  - Geographic distribution for latency reduction
- [ ] **Redis caching layer** (rate data, merchant lookup, invoice status)
- [ ] Docker / docker-compose production deployment
- [ ] Kubernetes manifests for auto-scaling
- [ ] Audit logging (who accessed what, when -- without logging PII)

## Phase 5 -- Monetization (Open Core + SaaS)

- [ ] **Self-hosted** (free, open source, BTCPay Server model)
  - Full feature parity, run your own CipherPay + Zebra node
  - Community support via GitHub issues
- [ ] **Hosted SaaS tiers**
  - Free tier: 50 invoices/month, community support
  - Pro tier: unlimited invoices, priority webhook delivery, dashboard
  - Enterprise: dedicated infrastructure, SLA, custom integrations
- [ ] API key rate limiting per tier
- [ ] Merchant dashboard (invoice history, analytics, webhook logs)
- [ ] Multi-merchant management (agencies managing multiple stores)

---

## CipherScan Improvements (for CipherPay support)

These are changes needed in the CipherScan explorer/indexer to support CipherPay at scale:

- [x] `GET /api/tx/{txid}/raw` -- raw hex endpoint for trial decryption
- [ ] `POST /api/tx/raw/batch` -- batch raw hex endpoint (Phase 2)
- [ ] `ws://api.cipherscan.app/mempool/stream` -- WebSocket mempool stream (Phase 2)
- [ ] Multi-Zebra-node infrastructure with load balancing (Phase 4)
- [ ] Rate limit tiers for API consumers (Phase 5)
- [ ] Dedicated CipherPay API key with higher rate limits (Phase 5)
- [ ] Raw tx hex storage in Postgres indexer (avoid RPC round-trips) (Phase 4)

---

## Design Principles

1. **Non-custodial**: CipherPay never holds funds. Merchants provide viewing keys.
2. **Privacy-first**: Shielded transactions only. No transparent address support.
3. **Data minimization**: Delete what you don't need. Encrypt what you must keep.
4. **Self-hostable**: Any merchant can run their own instance.
5. **CipherScan-native**: Leverages existing infrastructure, doesn't duplicate it.
