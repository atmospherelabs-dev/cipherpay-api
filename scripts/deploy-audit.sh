#!/bin/bash
# Run only after testing this candidate against a consistent production database copy.
set -euo pipefail
release=/opt/cipherpay-api/releases/audit-20260908
test "$(id -u)" = 0
test -f "$release/build-success"
test -x "$release/cipherpay"
test -f "$release/migration-verified"
python3 /opt/cipherpay-api/scripts/backup.py
systemctl stop cipherpay-api.service
trap 'trap - ERR; bash "$release/source/scripts/rollback-audit.sh"' ERR
python3 "$release/source/scripts/repair-orphan-payments.py" archive-clean \
    /opt/cipherpay-api/cipherpay.db "$release/rollback/orphan-payments.json"
if ! (cd "$release" && DATABASE_URL=sqlite:/opt/cipherpay-api/cipherpay.db ./cipherpay --check-database > migration-live.log 2>&1); then
    bash "$release/source/scripts/rollback-audit.sh"
    exit 1
fi
python3 "$release/source/scripts/repair-orphan-payments.py" record-consumption \
    /opt/cipherpay-api/cipherpay.db "$release/rollback/orphan-payments.json"
for database_file in /opt/cipherpay-api/cipherpay.db /opt/cipherpay-api/cipherpay.db-wal /opt/cipherpay-api/cipherpay.db-shm; do
    if test -f "$database_file"; then chown cipherpay:cipherpay "$database_file"; fi
done
chgrp cipherpay "$release"
chmod 750 "$release"
chmod 755 "$release/cipherpay"
install -d -m 755 /etc/systemd/system/cipherpay-api.service.d
cat > /etc/systemd/system/cipherpay-api.service.d/audit-release.conf <<EOF
[Service]
ExecStart=
ExecStart=$release/cipherpay
EOF
systemctl daemon-reload
if ! systemctl restart cipherpay-api.service; then
    bash "$release/source/scripts/rollback-audit.sh"
    exit 1
fi
for attempt in $(seq 1 12); do
    if curl --silent --fail --max-time 5 http://127.0.0.1:3080/api/health > "$release/health-after.json"; then
        if python3 - "$release/health-after.json" <<'PY'
import json,sys
data=json.load(open(sys.argv[1]))
sys.exit(0 if data.get('status') in ('ok','healthy','degraded') else 1)
PY
        then
            echo 'Candidate is serving health requests; verify scanner progress next.'
            exit 0
        fi
    fi
    sleep 3
done
bash "$release/source/scripts/rollback-audit.sh"
echo 'Candidate health check failed; previous version restored.' >&2
exit 1
