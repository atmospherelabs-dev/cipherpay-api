# Code Reference — Files to Have Open During the Workshop

Open these files in your editor before the session. Each section maps to a part of the talk.

## Part 2: Backend Interaction

### WebSocket Connection to CipherScan
**File:** `src/scanner/ws.rs`
**Lines:** 28-60
**What to show:** The `run()` function — connects to CipherScan WebSocket, subscribes to `raw_mempool`, forwards transactions to the scanner channel. Point out the auto-reconnect with exponential backoff (lines 5-6: `RECONNECT_MIN_SECS: 3`, `RECONNECT_MAX_SECS: 30`).

### Scanner Entry Point
**File:** `src/scanner/mod.rs`
**Lines:** 32-70
**What to show:** The `run()` function — initializes the scanner, sets up the WebSocket channel, spawns block scanning and mempool polling tasks. Point out the `SeenTxids` deduplication map (line 21-24).

## Part 3: Payment Flow

### Invoice Creation
**File:** `src/invoices/mod.rs`
**Lines:** 23-60
**What to show:** `create_invoice()` — generates memo code, calculates ZEC price from fiat, derives diversified payment address, creates Zcash URI. This is step 1-2 of the flow.

### Trial Decryption (the core primitive)
**File:** `src/scanner/decrypt.rs`
**Lines:** 115-165
**What to show:** `try_decrypt_with_keys()` — takes raw transaction hex and pre-computed keys, parses the Orchard bundle, trial-decrypts each action. This is the heart of CipherPay — where shielded payments become visible to the merchant.

Key lines to highlight:
- Line 122: `Transaction::read(&mut cursor, BranchId::Nu6_2)` — parsing raw tx bytes
- Line 127-128: Getting the Orchard bundle
- The `try_note_decryption` call — the actual trial decryption attempt
- The `DecryptedOutput` struct — what we extract (amount, memo)

### Mempool Transaction Processing
**File:** `src/scanner/mod.rs`
**Lines:** 390-430
**What to show:** `process_ws_mempool_tx()` — receives a raw mempool transaction from the WebSocket, loads pending invoices, trial-decrypts against all merchant keys, and calls `apply_mempool_invoice_totals()` on match.

### Block Scanning and Confirmation
**File:** `src/scanner/mod.rs`
**Lines:** 461-520
**What to show:** `scan_blocks()` — polls for new blocks, confirms detected transactions, captures fiat rate at confirmation. Point out the `confirmed_fiat()` helper that snapshots the exchange rate.

### Webhook Dispatch
**File:** `src/webhooks/mod.rs`
**Lines:** 9-19, 31-50
**What to show:**
1. `sign_payload()` (line 13) — HMAC-SHA256 signing: `timestamp.payload`
2. `retry_delay_secs()` (line 21) — exponential retry schedule
3. `dispatch_payment()` (line 31) — loads merchant webhook config, builds payload, signs, delivers, stores delivery record

### Key Cache (optimization)
**File:** `src/scanner/mod.rs`
**Lines:** 27-30
**What to show:** `KeyCache` struct — pre-computed decryption keys for all merchants. Refreshed only when the merchant set changes, not on every transaction. This is what makes scanning fast even with many merchants.

## Part 4: Open Source

### x402 Middleware
**File (different repo):** `cipherpay-x402/src/middleware.ts`
**Lines:** 151-283
**What to show:** `createPaywall()` — single function that handles x402, MPP, and session tokens. Auto-detects protocol from headers. Point out how simple the developer-facing API is vs. the complexity it hides.

### MCP Server
**File (different repo):** `cipherpay-mcp/src/index.ts`
**What to show:** The tool definitions — 8 tools an AI agent can call (rates, create invoice, check invoice, verify payment, open session, etc.).

## Quick Commands

```bash
# SSH and watch scanner logs live
ssh cipherpay-mainnet "journalctl -u cipherpay-api -f --no-hostname | grep -E 'scanner|Payment|WebSocket|WS'"

# Check current scanner state
ssh cipherpay-mainnet "journalctl -u cipherpay-api --no-hostname -n 20"

# Create a test invoice via API
curl -s https://api.cipherpay.app/api/invoices \
  -H "Authorization: Bearer $DASHBOARD_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"amount": 100, "currency": "USD", "product_name": "Workshop Test"}' | jq .
```
