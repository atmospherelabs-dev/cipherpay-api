# CipherPay Testnet Guide

End-to-end testing on Zcash testnet with real shielded transactions.

## What You Need

| Role | What | Why |
|------|------|-----|
| **Merchant wallet** | UFVK + Unified Address | CipherPay uses the UFVK to decrypt memos, and displays the address to buyers |
| **Buyer wallet** | Wallet with testnet ZEC | To send a shielded payment with the memo code |

You can use the same wallet app for both (different accounts), or two different apps.

## Step 1: Get Testnet Wallets

### Option A: YWallet (recommended — has UFVK export)

1. Download YWallet from [ywallet.app](https://ywallet.app)
2. Create a **new wallet** and select **Testnet**
3. Go to **Backup → Seed & Keys** to find your UFVK (`uviewtest1...`)
4. Your payment address is your Unified Address (`utest1...`)
5. Create a second account (or second wallet) for the "buyer" role

### Option B: Zashi

1. Download Zashi from [zashi.app](https://electriccoin.co/zashi)
2. Switch to testnet in settings (if available)
3. Your receiving address is the Unified Address
4. Note: UFVK export may require advanced settings

### Option C: zcash-cli (if running a testnet node)

```bash
# Generate a new address
zcash-cli -testnet z_getnewaddress

# Export the viewing key
zcash-cli -testnet z_exportviewingkey "YOUR_ADDRESS"
```

## Step 2: Get Testnet ZEC

Get free testnet ZEC (TAZ) from the faucet:

- **Zecpages Faucet**: [faucet.zecpages.com](https://faucet.zecpages.com/)

Request a small amount (0.1 TAZ is plenty for testing). Send it to your **buyer** wallet address.

## Step 3: Configure CipherPay

1. Start the server:
   ```bash
   cd cipherpay
   RUST_LOG=cipherpay=debug cargo run
   ```

2. Open `http://127.0.0.1:3080` in your browser

3. **Register Merchant**:
   - Paste your merchant wallet's **UFVK** (`uviewtest1...`)
   - Paste your merchant wallet's **Unified Address** (`utest1...`)
   - Click REGISTER MERCHANT

4. **Create Invoice**:
   - Select a product or use Custom Amount (use something tiny like €0.50)
   - Click CREATE INVOICE

## Step 4: Send the Payment

1. Open the checkout preview — note the:
   - **Payment address** (or scan the QR code)
   - **Memo code** (e.g. `CP-A7F3B2C1`)
   - **ZEC amount**

2. In your **buyer** wallet:
   - Send the displayed ZEC amount to the merchant address
   - **Include the memo code** in the memo field (this is critical)
   - Use a shielded (Orchard) transaction

3. Wait for detection:
   - **~5 seconds**: CipherPay detects the tx in the mempool → status becomes `DETECTED`
   - **~75 seconds**: Transaction gets mined → status becomes `CONFIRMED`

## How It Works Under the Hood

```
Buyer sends ZEC → Mempool → CipherPay Scanner polls every 5s
                                    ↓
                    Fetches raw tx hex from CipherScan API
                                    ↓
                    Trial-decrypts with merchant's UFVK
                                    ↓
                    Memo matches invoice? → DETECTED!
                                    ↓
                    Block mined? → CONFIRMED!
```

The scanner:
1. Polls `api.testnet.cipherscan.app/api/mempool` for new transaction IDs
2. Fetches raw transaction hex via `api.testnet.cipherscan.app/api/tx/{txid}/raw`
3. Parses the transaction, extracts Orchard actions
4. Trial-decrypts each action using the merchant's UFVK (Orchard FVK)
5. If decryption succeeds, extracts the memo text
6. If memo contains an active invoice's memo code → marks as detected
7. Polls `api.testnet.cipherscan.app/api/tx/{txid}` to check for block inclusion

## Troubleshooting

### "Price feed unavailable"
CoinGecko API may be rate-limited. CipherPay falls back to ~220 EUR/~240 USD per ZEC.

### Scanner not detecting payment
- Check the server logs (`RUST_LOG=cipherpay=debug`)
- Verify the UFVK matches the receiving address
- Ensure the memo code is exact (case-sensitive)
- Ensure the transaction is Orchard-shielded (not transparent)
- Check that `api.testnet.cipherscan.app` is reachable

### Transaction detected but not confirmed
- Testnet blocks mine every ~75 seconds
- The block scanner polls every 15 seconds
- Wait up to 2 minutes for confirmation

## Architecture Notes

- CipherPay does NOT run a Zcash node — it uses CipherScan's existing APIs
- Trial decryption runs in native Rust (~1-5ms per tx, vs ~50-100ms in WASM)
- The UFVK never leaves the server — same trust model as Stripe API keys
- For sovereign privacy: self-host CipherPay, point scanner at your own CipherScan instance
