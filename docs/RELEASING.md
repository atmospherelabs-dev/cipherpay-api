# Releasing the CipherPay API

Production runs a CI-built binary from a commit on `main`, never a binary built
from a working tree. Every deployed release records its commit in
`release-info.json`, so `status.sh` can prove what is serving.

## Flow

1. **Merge to `main`.** `.github/workflows/release-build.yml` runs the tests,
   builds on Ubuntu 24.04 (matching the hosts) and publishes a `build-<sha>`
   prerelease with the binary, source archive, `build-info.json` and
   `SHA256SUMS`.
2. **Stage on the host** (service keeps running):

   ```bash
   sudo /opt/cipherpay-api/releases/<current>/source/scripts/release/stage.sh <full-sha>
   ```

   Refuses commits not on `main`, verifies checksums, migrates a private copy
   of the live database and asserts every existing table/column is unchanged.
   Output: `/opt/cipherpay-api/releases/<YYYYMMDD>-<sha7>/`.
3. **Activate:**

   ```bash
   sudo <release>/source/scripts/release/activate.sh <release>
   ```

   Takes a consistent database backup, stops the service, runs
   `--check-database` on the live database, points
   `cipherpay-api.service.d/zz-release.conf` at the new binary and waits for a
   healthy `/api/health`. Any failure restores the previous drop-in/binary and
   keeps current payment data.
4. **Check drift any time:**

   ```bash
   sudo <release>/source/scripts/release/status.sh
   ```

   Exit 0 means serving `main` (or an older `main` commit); exit 2 means dirty,
   unknown or not on `main`.

## Rules

- Do not deploy uncommitted changes or branch builds. Land them on `main` first.
- Migrations must be additive; `validate-database.py` rejects changes to
  existing rows.
- Environment files are untouched by these scripts. The historical
  `zz-crosschain-20261021.conf` drop-in still supplies the NEAR cross-chain
  `EnvironmentFile`; move it to a stable path before removing that release.
- Testnet (`/root/cipherpay-api-testnet`, `cipherpay-testnet.service`) still
  uses a source checkout and is not covered by these scripts yet.
