# CipherPay Scalability Strategy

## The Fundamental Constraint

CipherPay is a **non-custodial, multi-tenant shielded payment processor**. Unlike Stripe (custodial, reference-number matching) or BTCPay (non-custodial, one instance per merchant), CipherPay must **trial-decrypt every shielded transaction against every merchant's viewing key**.

This makes the core scan loop `O(transactions × merchants)` — the computational cost of privacy absorbed server-side.

---

## Current Architecture

```
┌────────────────────────────────────────────────────────┐
│  Single Rust Process                                   │
│                                                        │
│  ┌──────────────┐     ┌──────────────┐                 │
│  │ Mempool Scan  │     │ Block Scan   │                 │
│  │ (every ~10s)  │     │ (every ~30s) │                 │
│  └──────┬───────┘     └──────┬───────┘                 │
│         │                    │                          │
│         ▼                    ▼                          │
│  For each tx × each merchant:                          │
│    trial_decrypt(raw_tx, merchant_ufvk)                 │
│    → match against pending invoices                     │
│    → mark_detected / mark_confirmed                     │
│    → dispatch_webhook (SYNC — blocks loop)              │
│                                                        │
│  ┌──────────────┐                                      │
│  │ SQLite (WAL) │  single-writer, file-based            │
│  └──────────────┘                                      │
│                                                        │
│  ┌──────────────┐                                      │
│  │ API (actix)  │  SSE streams, REST endpoints          │
│  └──────────────┘                                      │
└────────────────────────────────────────────────────────┘
         │ HTTP polling
         ▼
┌────────────────────┐
│ CipherScan + zcashd│
│ (blockchain data)  │
└────────────────────┘
```

### Current Capacity Estimate

| Merchants | Mempool txs/cycle | Decryptions/cycle | Time @ 5ms each | Status |
|-----------|-------------------|-------------------|-----------------|--------|
| 5         | 15                | 75                | 0.4s            | Easy   |
| 20        | 15                | 300               | 1.5s            | Fine   |
| 100       | 15                | 1,500             | 7.5s            | Tight  |
| 500       | 15                | 7,500             | 37s             | Broken |

Zcash's low shielded transaction volume (~5-30 txs/block, 75s block time) is our biggest asset.

---

## Known Risks

### 1. Synchronous Webhook Delivery
Webhook HTTP POST (10s timeout) blocks the scan loop. One slow merchant server stalls everything.

### 2. In-Memory `last_height`
If the process crashes, `last_height` resets. Blocks mined during downtime are never scanned. Payments during that window are lost.

### 3. In-Memory `seen_txids`
On restart, the entire mempool is re-processed. Combined with async webhooks, this can cause duplicate webhook deliveries.

### 4. PIVK Re-derivation
`PreparedIncomingViewingKey` is re-computed from the UFVK on every scan cycle for every merchant. This involves scalar multiplications on the Pallas curve — wasted CPU.

### 5. SQLite Single-Writer
All writes serialize: `mark_detected`, `INSERT webhook_deliveries`, `UPDATE webhook_deliveries`. Fine at low volume, contention at high volume.

---

## Optimization Tiers

### Tier 1 — No Infrastructure Change (implement now)

These fixes stay on the same server, same SQLite, same process. They raise the ceiling from ~50 to ~500 merchants.

#### 1.1 Async Webhook Delivery
**Problem:** `dispatch_payment()` does HTTP POST inline, blocking the scan loop.
**Fix:** `tokio::spawn` the webhook delivery. Scanner writes to `webhook_deliveries` and moves on.
**Warning:** The spawned task must not share a database transaction with the scanner. Use the connection pool independently.

#### 1.2 Cache `PreparedIncomingViewingKey`
**Problem:** Re-deriving PIVKs from UFVKs every cycle wastes CPU on curve operations.
**Fix:** Compute PIVKs once on startup, store in a `HashMap<merchant_id, PreparedIVKs>`. Invalidate/update only when a merchant registers or updates their UFVK.

#### 1.3 Persist `last_height`
**Problem:** In-memory `last_height` resets on crash; blocks during downtime are missed.
**Fix:** Store in a `scanner_state` table. Read on startup, write after each successful block scan.

#### 1.4 Persist `seen_txids`
**Problem:** In-memory HashSet; restart causes full mempool re-processing + potential duplicate webhooks.
**Fix:** Store in DB or a local file. Prune entries older than 1 hour (mempool TTL).

### Tier 2 — Parallelism (when scan cycles exceed 5s)

#### 2.1 Parallel Decryption with Rayon
**Problem:** Trial decryption is single-threaded.
**Fix:** Use `rayon::par_iter()` across merchants for each transaction.
**Critical:** Wrap in `tokio::task::spawn_blocking` — Rayon uses blocking threads that would starve Tokio's async runtime (freezing SSE streams and webhook delivery).
**Expected gain:** Near-linear speedup with core count (8 cores → ~7-8x faster).

#### 2.2 Merchant-Scoped Invoice Index
**Problem:** `find_matching_invoice` linearly scans all pending invoices.
**Fix:** Maintain `HashMap<orchard_receiver_hex, Invoice>` for O(1) lookup after decryption.

### Tier 3 — Database Migration (when writes contend)

#### 3.1 PostgreSQL
**When:** Write contention on SQLite becomes measurable (likely 200+ merchants with async webhooks all writing concurrently).
**Why:** Concurrent writers, connection pooling, better tooling for monitoring/backups.
**Note:** Don't rush here. SQLite handles thousands of micro-writes/second in WAL mode.

### Tier 4 — Architecture Split (500+ merchants)

#### 4.1 Separate API from Scanner
**Why:** The API (stateless HTTP) can scale horizontally. The scanner (stateful, exactly-one) cannot.
**Setup:** API instances behind nginx/caddy, single scanner process, shared PostgreSQL.

#### 4.2 Push Model from CipherScan
**Why:** Eliminates polling overhead. CipherScan pushes new raw txs via WebSocket/SSE.
**Benefit:** Lower latency, less wasted work fetching unchanged mempool state.

#### 4.3 Sharded Decryption Workers
**Why:** When one server's CPU can't handle all merchants.
**Setup:** Partition merchants across worker processes, each responsible for a subset of UFVKs.
**Constraint:** Each merchant belongs to exactly one worker (no duplicate processing).

---

## Infrastructure Roadmap

| Stage | Merchants | Server | Database | Cost/mo |
|-------|-----------|--------|----------|---------|
| **POC** (now) | 1-5 | $6 VPS, 1 core | SQLite | ~$6 |
| **Early mainnet** | 5-20 | $20-40 VPS, 2-4 cores | SQLite | ~$40-80 |
| **Growth** | 20-100 | $40-80 VPS, 4 cores | PostgreSQL | ~$80-120 |
| **Scale** | 100-500 | Split API + Scanner | Managed Postgres | ~$200-400 |
| **Enterprise** | 500+ | Multiple workers | Sharded Postgres | $400+ |

### Key Architectural Invariant

**The scanner must be a single instance.** Two scanners processing the same transactions would cause double webhook deliveries, double stock decrements, and corrupted state. If you need redundancy, use active/passive failover, not load balancing.

---

## Comparison with Industry

| Aspect | Stripe | BTCPay Server | CipherPay |
|--------|--------|---------------|-----------|
| Custody | Custodial | Non-custodial | Non-custodial |
| Privacy | None (KYC) | Pseudonymous | Fully shielded |
| Matching | Reference lookup O(1) | Address watch O(1) | Trial decrypt O(n) |
| Multi-tenant | Yes (sharded) | No (per-merchant) | Yes (single instance) |
| Scaling model | Horizontal | Per-merchant | Vertical → sharded |
| Payment detection | Push (bank network) | Push (bitcoind ZMQ) | Poll (mempool API) |

CipherPay occupies a unique position: **multi-tenant non-custodial shielded payment detection**. The trial decryption cost is the fundamental tradeoff of privacy. Zcash's low transaction volume makes this viable at meaningful scale.

---

## Decision Log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2026-02-21 | Document scalability strategy | Capture research before it's forgotten |
| | Implement Tier 1 optimizations | Biggest risk/reward ratio, no infra change needed |
| | Defer Rayon parallelism | Not needed at current merchant count |
| | Defer PostgreSQL migration | SQLite handles current load; premature complexity |
