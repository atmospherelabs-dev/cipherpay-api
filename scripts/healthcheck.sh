#!/usr/bin/env bash
# CipherPay health monitor — run via cron every 5 minutes:
#   */5 * * * * /opt/cipherpay-api/scripts/healthcheck.sh
#
# Optional: set ALERT_WEBHOOK to a Telegram bot URL or any webhook:
#   export ALERT_WEBHOOK="https://api.telegram.org/bot<TOKEN>/sendMessage?chat_id=<CHAT_ID>&text="

API_URL="${CIPHERPAY_API_URL:-http://127.0.0.1:3080}"
ALERT_WEBHOOK="${ALERT_WEBHOOK:-}"
STATE_FILE="/tmp/cipherpay-health-state"

response=$(curl -sf --max-time 10 "${API_URL}/api/health" 2>/dev/null)
exit_code=$?

if [ $exit_code -ne 0 ]; then
    status="unreachable"
    message="CipherPay API is unreachable (curl exit $exit_code)"
else
    status=$(echo "$response" | jq -r '.status // "unknown"' 2>/dev/null)
    if [ "$status" = "unhealthy" ]; then
        checks=$(echo "$response" | jq -c '.checks' 2>/dev/null)
        message="CipherPay UNHEALTHY: $checks"
    elif [ "$status" = "degraded" ]; then
        checks=$(echo "$response" | jq -c '.checks' 2>/dev/null)
        message="CipherPay DEGRADED: $checks"
    fi
fi

prev_status="ok"
[ -f "$STATE_FILE" ] && prev_status=$(cat "$STATE_FILE")

if [ "$status" = "ok" ]; then
    if [ "$prev_status" != "ok" ]; then
        message="CipherPay recovered — status OK"
        logger -t cipherpay-health "$message"
        if [ -n "$ALERT_WEBHOOK" ]; then
            curl -sf --max-time 5 "${ALERT_WEBHOOK}$(echo "$message" | jq -sRr @uri)" >/dev/null 2>&1
        fi
    fi
    echo "ok" > "$STATE_FILE"
    exit 0
fi

echo "$status" > "$STATE_FILE"
logger -t cipherpay-health "$message"

# Only alert on state transitions to avoid spam
if [ "$prev_status" = "ok" ] || [ "$prev_status" != "$status" ]; then
    if [ -n "$ALERT_WEBHOOK" ]; then
        curl -sf --max-time 5 "${ALERT_WEBHOOK}$(echo "$message" | jq -sRr @uri)" >/dev/null 2>&1
    fi
fi
