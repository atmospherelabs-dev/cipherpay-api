# Live Demo Script

Run this during **Part 3** of the presentation (after explaining the payment flow on slides).

## Pre-Demo Setup (do 10 minutes before the session)

1. **Browser tabs open:**
   - Tab 1: `cipherpay.app` dashboard (logged in as Shopify Example or test merchant)
   - Tab 2: `cipherpay.app/en/pay/{invoice-id}` (will open after creating invoice)
   - Tab 3: Terminal with live scanner logs (SSH into server)

2. **Terminal ready:**
   ```bash
   ssh cipherpay-mainnet "journalctl -u cipherpay-api -f --no-hostname"
   ```
   This shows real-time scanner logs. Key lines to point out:
   - `[WS] Subscribed to raw_mempool` — WebSocket is connected
   - `Price feed updated` — exchange rates are live
   - `Payment detected` — the moment a payment is found

3. **Wallet ready:**
   - Zashi or Zipher on phone, funded with testnet or small mainnet amount
   - Have the wallet app visible / screen-shareable

4. **Fallback:** If the live payment takes too long or fails, have a screenshot/recording of a successful detection ready.

## Demo Flow (5 minutes)

### Step 1: Create an Invoice (30 seconds)
- Go to Dashboard > Invoices
- Click "Payment Link" or use the API tab on the landing page
- Show the `curl` command:
  ```bash
  curl -X POST https://api.cipherpay.app/api/invoices \
    -H "Authorization: Bearer YOUR_TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"amount": 500, "currency": "USD", "product_name": "Workshop Demo"}'
  ```
- **Point out:** Invoice ID, memo code (`CP-XXXXXXXX`), payment address, QR code

### Step 2: Show the Payment Page (15 seconds)
- Open the checkout page in browser
- **Point out:** QR code, Zcash URI, amount in ZEC, countdown timer
- "This is what the buyer sees"

### Step 3: Pay from Wallet (30 seconds)
- Scan the QR code from Zashi/Zipher
- Send the payment
- "Now watch the dashboard and the terminal..."

### Step 4: Watch Detection (30 seconds - 2 minutes)
- **Terminal:** Wait for `Payment detected` log line
- **Dashboard:** Invoice status changes from `pending` to `detected`
- **Point out the speed:** "That was sub-second. The transaction hit the mempool, CipherScan pushed the raw hex via WebSocket, CipherPay trial-decrypted it, and matched it to this invoice."

### Step 5: Explain What Happened (1 minute)
- "Behind the scenes:"
  1. The transaction entered the Zcash mempool
  2. CipherScan saw it and pushed the raw hex over WebSocket
  3. CipherPay's scanner received it, trial-decrypted every Orchard output against the merchant's IVK
  4. One output matched — payment detected
  5. Webhook fired to the merchant's endpoint
  6. When it confirms in a block, fiat rate is captured for accounting

## Talking Points During the Wait

If there's a delay (network, wallet sync), fill with:
- "In production, we also have a polling fallback in case the WebSocket drops"
- "The scanner caches pre-computed decryption keys so it doesn't re-derive them every time"
- "Each invoice gets a unique diversified address — no memo matching needed"

## Cleanup

No cleanup needed — the invoice will expire naturally (default 15 minutes).
