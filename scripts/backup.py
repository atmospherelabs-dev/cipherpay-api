#!/usr/bin/env python3
"""Consistent SQLite backup; verify every snapshot before retention cleanup."""
import os, pathlib, sqlite3, datetime
os.umask(0o077)
root=pathlib.Path('/opt/cipherpay-api')
backup=root/'backups';backup.mkdir(mode=0o700,exist_ok=True)
now=datetime.datetime.now(datetime.timezone.utc)
file=backup/('cipherpay-'+now.strftime('%Y%m%dT%H%M%SZ')+'.db')
with sqlite3.connect(root/'cipherpay.db') as source, sqlite3.connect(file) as dest:
    source.backup(dest)
    if dest.execute('pragma integrity_check').fetchone()[0]!='ok': raise RuntimeError('Backup integrity check failed')
# 14 daily backups; an operator may archive a snapshot off-host separately.
for old in backup.glob('cipherpay-*.db'):
    if now.timestamp()-old.stat().st_mtime>14*86400: old.unlink()
print('Verified SQLite backup created')
