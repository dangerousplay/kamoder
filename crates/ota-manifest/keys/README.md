# Update signing keys

| File | What |
|---|---|
| `kamoder.pub` | Project update key ("current"). **Placeholder** until the maintainer generates the key. |
| `kamoder-next.pub` | Rotation key ("next"). Placeholder unless a rotation is in progress. |
| `test/test.pub`, `test/test.key` | TEST-ONLY key pair, secret committed on purpose. Trusted only by tests (`cfg(test)` or the `test-key` feature); signs `tests/data/` fixtures. |
| `test/other.pub`, `test/other.key` | TEST-ONLY unrelated key pair for wrong-key tests. |

Lines starting with `#` are ignored. A build with no real key trusts nothing:
every manifest is rejected with "no trusted update key built in".
`KAMODER_UPDATE_PUBKEY` / `KAMODER_UPDATE_PUBKEY_NEXT` at build time (the
`RW...` line or the whole `.pub` file) override the files.

## Maintainer: create the project key (once)

```sh
minisign -G -p kamoder.pub -s kamoder.key        # set a password
cp kamoder.pub crates/ota-manifest/keys/kamoder.pub
./x test -p ota-manifest                         # checks the key parses
git add crates/ota-manifest/keys/kamoder.pub     # commit the PUBLIC key only
```

Then in GitHub, Settings > Secrets and variables > Actions, add
`MINISIGN_SECRET_KEY` (contents of `kamoder.key`) and `MINISIGN_PASSWORD`
(used by the release workflow). Keep `kamoder.key` offline and never commit it.

## Rotation

1. `minisign -G` a new pair; put the new public key in `kamoder-next.pub`.
2. Release once, still signed with the old key (devices now trust both).
3. Switch the GitHub secret to the new key; move the new public key to
   `kamoder.pub` and restore the `kamoder-next.pub` placeholder.
