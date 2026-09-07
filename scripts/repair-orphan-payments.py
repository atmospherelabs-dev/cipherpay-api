#!/usr/bin/env python3
"""Archive detached ledger rows, then retain their txids after schema migration.

Run archive-clean only with the API stopped or against an isolated database copy.
The archive must be retained privately with the release backup.
"""
import json
import os
import pathlib
import sqlite3
import sys

mode, database, archive_name = sys.argv[1:]
archive = pathlib.Path(archive_name)
os.umask(0o077)
with sqlite3.connect(database) as db:
    db.row_factory = sqlite3.Row
    if mode == 'archive-clean':
        db.execute('BEGIN IMMEDIATE')
        rows = [dict(row) for row in db.execute(
            'SELECT p.* FROM invoice_payments p WHERE NOT EXISTS '
            '(SELECT 1 FROM invoices i WHERE i.id=p.invoice_id)')]
        # Never overwrite an earlier archive during a retry.
        if rows:
            with archive.open('x') as output:
                json.dump(rows, output)
                output.flush()
                os.fsync(output.fileno())
            deleted = db.execute('DELETE FROM invoice_payments WHERE NOT EXISTS '
                '(SELECT 1 FROM invoices i WHERE i.id=invoice_payments.invoice_id)').rowcount
            assert deleted == len(rows)
        print(f'Archived and removed {len(rows)} detached ledger rows')
    elif mode == 'record-consumption':
        rows = json.loads(archive.read_text()) if archive.exists() else []
        for row in rows:
            db.execute('INSERT OR IGNORE INTO payment_consumptions (txid,purpose) VALUES (?,?)',
                       (row['txid'].lower(), 'orphaned-invoice'))
        print(f'Retained anti-replay evidence for {len(rows)} detached payments')
    else:
        raise ValueError('Expected archive-clean or record-consumption')
