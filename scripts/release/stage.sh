#!/bin/bash
# Stage a CI-built release on a CipherPay host without touching the running service.
#
#   sudo scripts/release/stage.sh <full-commit-sha>
#
# Refuses commits that are not on main, verifies the CI checksums, then migrates
# a private copy of the live database and proves existing data is unchanged.
# Activate afterwards with scripts/release/activate.sh <release-dir>.
set -euo pipefail

repo=atmospherelabs-dev/cipherpay-api
root=/opt/cipherpay-api
sha=${1:?usage: stage.sh <full-commit-sha>}
[[ $sha =~ ^[0-9a-f]{40}$ ]] || { echo "Pass the full 40-character commit SHA" >&2; exit 1; }
test "$(id -u)" = 0

# Only commits reachable from main may ship: main must be identical to or ahead of $sha.
status=$(curl -fsS "https://api.github.com/repos/$repo/compare/$sha...main" | jq -r .status)
case $status in
    identical|ahead) ;;
    *) echo "Commit $sha is not on main (compare status: $status)" >&2; exit 1 ;;
esac

release=$root/releases/$(date -u +%Y%m%d)-${sha:0:7}
test ! -e "$release" || { echo "$release already exists" >&2; exit 1; }
install -d -m 750 -o root -g cipherpay "$release"
cd "$release"

base=https://github.com/$repo/releases/download/build-$sha
for asset in cipherpay-linux-x86_64 source.tar.gz build-info.json SHA256SUMS; do
    curl -fsSL -o "$asset" "$base/$asset"
done
sha256sum --check --strict SHA256SUMS
test "$(jq -r .source_head build-info.json)" = "$sha"
test "$(jq -r .dirty build-info.json)" = false
mv cipherpay-linux-x86_64 cipherpay
chmod 755 cipherpay
mkdir source && tar -xzf source.tar.gz -C source

python3 "$release/source/scripts/release/validate-database.py" "$release"

jq -n --arg sha "$sha" --arg release "$release" \
      --arg binary_sha256 "$(sha256sum cipherpay | cut -d' ' -f1)" \
      --arg run_url "$(jq -r .run_url build-info.json)" \
      --arg staged_at "$(date -u +%FT%TZ)" \
      '{source_head: $sha, dirty: false, release_path: $release, binary_sha256: $binary_sha256,
        build_run: $run_url, staged_at: $staged_at}' > release-info.json
echo "Staged $release. Activate with: scripts/release/activate.sh $release"
