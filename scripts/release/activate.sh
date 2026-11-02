#!/bin/bash
# Switch cipherpay-api to a release staged by stage.sh; restore the previous
# binary (keeping current payment data) if any step fails.
#
#   sudo scripts/release/activate.sh /opt/cipherpay-api/releases/<release>
set -euo pipefail

release=$(realpath "${1:?usage: activate.sh <release-dir>}")
root=/opt/cipherpay-api
unitdir=/etc/systemd/system/cipherpay-api.service.d
# Sorts after the historical audit/subscriptions/crosschain drop-ins, so its
# ExecStart wins. EnvironmentFile entries from earlier drop-ins still apply.
dropin=$unitdir/zz-release.conf
test "$(id -u)" = 0
test -f "$release/migration-verified"
test -x "$release/cipherpay"
test "$(jq -r .dirty "$release/release-info.json")" = false

previous_exec=$(systemctl show -p ExecStart --value cipherpay-api.service | sed -n 's/.*path=\([^ ;]*\).*/\1/p')
echo "$previous_exec" > "$release/previous-binary"
if test -f "$dropin"; then cp -p "$dropin" "$release/previous-dropin.conf"; fi

python3 - "$release" <<'PY'
from pathlib import Path
import sqlite3, sys, os
os.umask(0o077)
r = Path(sys.argv[1])
with sqlite3.connect('/opt/cipherpay-api/cipherpay.db') as source, \
        sqlite3.connect(r / 'database-immediately-before.db') as backup:
    source.backup(backup)
    assert backup.execute('pragma integrity_check').fetchone()[0] == 'ok'
PY

systemctl stop cipherpay-api.service
recover() {
    trap - ERR
    if test -f "$release/previous-dropin.conf"; then
        cp -p "$release/previous-dropin.conf" "$dropin"
    else
        rm -f "$dropin"
    fi
    systemctl daemon-reload
    systemctl restart cipherpay-api.service
    echo "Activation failed; restored $previous_exec with current payment data retained." >&2
    exit 1
}
trap recover ERR

DATABASE_URL=sqlite:$root/cipherpay.db "$release/cipherpay" --check-database > "$release/database-live.log" 2>&1
for file in $root/cipherpay.db $root/cipherpay.db-wal $root/cipherpay.db-shm; do
    if test -f "$file"; then chown cipherpay:cipherpay "$file"; fi
done

install -d -m 755 "$unitdir"
cat > "$dropin" <<EOF
[Service]
ExecStart=
ExecStart=$release/cipherpay
EOF
chmod 644 "$dropin"
systemctl daemon-reload
systemctl start cipherpay-api.service

for _ in $(seq 1 18); do
    if curl --silent --fail --max-time 4 http://127.0.0.1:3080/api/health > "$release/health-after.json" &&
        jq -e '.checks.database == "ok" and (.status == "ok" or .status == "degraded")' \
            "$release/health-after.json" > /dev/null; then
        trap - ERR
        date -u +%FT%TZ > "$release/activated-at"
        echo "Activated $release (was $previous_exec). Verify scanner progress and checkout."
        exit 0
    fi
    sleep 2
done
false
