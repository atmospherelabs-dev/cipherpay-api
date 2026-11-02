#!/bin/bash
# Report which commit cipherpay-api is serving and whether it has drifted from main.
# Exits non-zero when the running build is dirty, unknown, or not on main.
set -euo pipefail

repo=atmospherelabs-dev/cipherpay-api
binary=$(systemctl show -p ExecStart --value cipherpay-api.service | sed -n 's/.*path=\([^ ;]*\).*/\1/p')
info=$(dirname "$binary")/release-info.json
echo "binary:  $binary"
if ! test -f "$info"; then
    echo "drift:   UNKNOWN (no release-info.json; not deployed by stage.sh)"
    exit 2
fi

sha=$(jq -r '.source_head // empty' "$info")
dirty=$(jq -r '.dirty // .source_dirty // "unknown"' "$info")
echo "commit:  $sha (dirty: $dirty)"
if test "$dirty" != false; then
    echo "drift:   DIRTY (running code is not a committed revision)"
    exit 2
fi

compare=$(curl -fsS "https://api.github.com/repos/$repo/compare/$sha...main")
status=$(jq -r .status <<< "$compare")
case $status in
    identical) echo "drift:   none (serving main)" ;;
    ahead)     echo "drift:   main is $(jq -r .ahead_by <<< "$compare") commit(s) ahead; stage the latest build" ;;
    *)         echo "drift:   NOT ON MAIN (compare status: $status)"; exit 2 ;;
esac
