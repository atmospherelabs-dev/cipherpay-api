#!/usr/bin/env python3
"""Migrate a private copy of the live database and prove existing columns keep their data.

Usage: validate-database.py <release-dir>

Migrations must be additive: every table and column that exists before the
candidate's --check-database run must hold identical rows afterwards.
"""
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys

release = Path(sys.argv[1]).resolve()
live = Path('/opt/cipherpay-api/cipherpay.db')
snapshot = release / 'database-before.db'
validation = release / 'validation.db'
os.umask(0o077)
assert not snapshot.exists(), 'Snapshot already exists; use a new release path'
with sqlite3.connect(live) as src, sqlite3.connect(snapshot) as dest:
    src.backup(dest)
with sqlite3.connect(snapshot) as src, sqlite3.connect(validation) as dest:
    src.backup(dest)


def quote(name):
    return '"' + name.replace('"', '""') + '"'


with sqlite3.connect(snapshot) as before:
    tables = [r[0] for r in before.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")]
    columns = {t: [r[1] for r in before.execute('PRAGMA table_info(' + quote(t) + ')')] for t in tables}

    def digest(db, table):
        rows = db.execute('SELECT ' + ','.join(map(quote, columns[table])) + ' FROM ' + quote(table)).fetchall()
        encoded = sorted(repr(row) for row in rows)
        return len(rows), hashlib.sha256('\n'.join(encoded).encode()).hexdigest()

    baseline = {t: digest(before, t) for t in tables if t not in ('schema_migrations', '_sqlx_migrations')}

environment = dict(os.environ, DATABASE_URL='sqlite:' + str(validation))
with (release / 'database-validation.log').open('w') as log:
    subprocess.run([str(release / 'cipherpay'), '--check-database'], cwd=release, env=environment,
                   stdout=log, stderr=subprocess.STDOUT, check=True)

with sqlite3.connect(validation) as after:
    assert after.execute('PRAGMA integrity_check').fetchone()[0] == 'ok'
    assert not after.execute('PRAGMA foreign_key_check').fetchall()
    changes = [table for table, old in baseline.items() if digest(after, table) != old]
    assert not changes, 'Existing data changed: ' + ', '.join(changes)
    invoice_count = after.execute('SELECT count(*) FROM invoices').fetchone()[0]

result = {'existing_tables_preserved': len(baseline), 'invoice_count': invoice_count,
          'integrity': 'ok', 'foreign_keys': 'ok'}
(release / 'database-validation.json').write_text(json.dumps(result, indent=2) + '\n')
(release / 'migration-verified').touch()
print(json.dumps(result))
