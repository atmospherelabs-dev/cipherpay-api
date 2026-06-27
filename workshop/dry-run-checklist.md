# Dry Run Checklist — June 29 (day before the workshop)

Do a complete end-to-end test payment the day before.

## Infrastructure Check

- [ ] API health: `curl https://api.cipherpay.app/api/health` returns `{"status":"ok"}`
- [ ] Scanner running: `ssh cipherpay-mainnet "systemctl status cipherpay-api"` shows `active (running)`
- [ ] WebSocket connected: logs show `[WS] Subscribed to raw_mempool`
- [ ] Price feed alive: logs show recent `Price feed updated` lines
- [ ] Frontend live: `cipherpay.app` loads, dashboard accessible

## Wallet Check

- [ ] Zashi or Zipher has mainnet ZEC (even 0.001 ZEC is enough)
- [ ] Wallet is synced and ready to send
- [ ] Camera/screen sharing can show the wallet app

## Presentation Check

- [ ] `slides.html` opens in browser and looks correct
- [ ] All slides render (navigate through all of them)
- [ ] Code blocks are syntax-highlighted and readable
- [ ] Font sizes are legible for screen sharing

## Live Payment Test

1. [ ] Open dashboard in browser, go to Invoices
2. [ ] Create a test invoice ($1 or equivalent)
3. [ ] Open the payment page in a separate tab
4. [ ] Open terminal with scanner logs: `ssh cipherpay-mainnet "journalctl -u cipherpay-api -f --no-hostname"`
5. [ ] Scan QR from wallet and send payment
6. [ ] Verify detection appears in:
   - [ ] Terminal logs (`Payment detected`)
   - [ ] Dashboard (invoice status changes to `detected`)
   - [ ] Payment page (SSE updates in real-time)
7. [ ] Note detection latency (should be sub-second to ~5 seconds)

## Known Issues

- **WebSocket drops:** The CipherScan WebSocket connection resets every ~4 minutes. Auto-reconnect takes 3 seconds. If the drop happens during the demo payment, detection falls back to polling (5-30 second delay). Not a problem — just mention it: "We have a polling fallback for exactly this case."

## Screen Layout for the Session

Suggested layout for screen sharing:
- Left: Slides (browser, full width)
- When showing code: Switch to editor with the files from `code-reference.md`
- When showing demo: Split browser (dashboard left) + terminal (scanner logs right)

## Timing

| Part | Duration | Content |
|------|----------|---------|
| 1. The Stack | 10 min | Slides: intro, architecture |
| 2. RPC & APIs | 15 min | Slides + code walkthrough |
| 3. Payment Flow | 20 min | Slides + live demo |
| 4. Open Source | 10 min | Slides: repos, npm, hackathon |
| 5. Q&A | 5 min | Open questions |
| **Total** | **60 min** | |
