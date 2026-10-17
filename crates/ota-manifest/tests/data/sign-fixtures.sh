#!/bin/sh
# Re-sign the signature-test fixtures with the committed TEST-ONLY keys
# (unencrypted, so no password prompt). Needs the `minisign` CLI.
# Run from anywhere after editing a fixture: sh tests/data/sign-fixtures.sh
set -eu
here=${0%/*}
keys=$here/../../keys/test
sign() { # file key out comment
    minisign -S -s "$keys/$2" -m "$here/$1" -x "$here/$3" -t "$4" >/dev/null
}
sign manifest.json test.key manifest.json.minisig "kamoder 0.2.0 seq=1760700000"
sign manifest.json other.key manifest.json.other.minisig "kamoder 0.2.0 seq=1760700000"
sign manifest.json test.key manifest.json.badcomment.minisig "kamoder 0.2.0 seq=1"
sign schema2.json test.key schema2.json.minisig "kamoder 0.2.0 seq=1760700000"
sign traversal.json test.key traversal.json.minisig "kamoder 0.2.0 seq=1760700000"
