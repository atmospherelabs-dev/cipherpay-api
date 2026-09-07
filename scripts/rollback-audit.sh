#!/bin/bash
# Restore the pre-audit binary without discarding payments received since deployment.
set -euo pipefail
release=/opt/cipherpay-api/releases/audit-20260908
test "$(id -u)" = 0
test -x "$release/rollback/cipherpay"
systemctl stop cipherpay-api.service
python3 - <<'PY'
import sqlite3
with sqlite3.connect('/opt/cipherpay-api/cipherpay.db') as db:
    db.execute('DROP TRIGGER IF EXISTS invoice_payment_outbox')
    db.execute("DELETE FROM schema_migrations WHERE name='payment_outbox_v2026_09_08'")
PY
install -m 755 "$release/rollback/cipherpay" /opt/cipherpay-api/target/release/cipherpay
rm -f /etc/systemd/system/cipherpay-api.service.d/audit-release.conf
systemctl daemon-reload
systemctl start cipherpay-api.service
echo 'Previous binary restored; current payment data retained.'
