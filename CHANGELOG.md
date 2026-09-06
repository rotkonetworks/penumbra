# Changelog

## 2.0.8

This release is backwards-compatible with the existing `v2.0.x` series and is
**not consensus-breaking**. Nodes running `v2.0.6`, `v2.0.7` and `v2.0.8` can
coexist on the network. `APP_VERSION` is unchanged at 11. There is no upgrade
height and no coordination is needed.

Penumbra is now maintained by Rotko Networks. Releases are published from
`rotkonetworks/penumbra`.

### New: `pd migrate prune`, chain-state pruning for node operators

`pd` now ships an offline pruning command that collapses the historical
versions of the Jellyfish Merkle Tree to the latest version. On mainnet this
reduces the `pd` database from roughly 350 GB to roughly 30 GB. The root hash
is unchanged and every pruned key is verified against it with range proofs
during the rebuild.

Measured on a copy of mainnet state at version 12,561,008: 350 GB to 30 GB in
59 minutes on a single core with 4.7 GB peak memory. The result was then run
under stock `pd 2.0.6` and CometBFT `0.37.16`, which handshook at the original
root hash and synced live mainnet blocks.

#### How to prune a validator

Prune validators **one at a time**. The node is offline for about an hour
while the prune runs. Taking more than a third of voting power offline at once
halts the chain.

1. Install the `pd` 2.0.8 binary (see below). It is a drop-in replacement.
2. Make sure at least 35 GB is free next to the `pd` data directory.
3. Stop `pd`. CometBFT may keep running; it will wait.
   ```sh
   sudo systemctl stop penumbra
   ```
4. Run the prune. `--home` is the `pd` home, the directory that contains
   `rocksdb`. If `PENUMBRA_PD_HOME` is set in the unit's environment, `--home`
   can be omitted.
   ```sh
   pd migrate --home /path/to/node0/pd prune
   ```
   The log ends with `JMT pruning complete root_hash=...`. That root hash must
   match the one printed at the start in `starting JMT pruning`.
5. Start `pd` and confirm it is signing again.
   ```sh
   sudo systemctl start penumbra
   ```
6. The unpruned database is kept at `<pd home>/rocksdb_old`. Once the node has
   been signing for a while, remove it to reclaim the space:
   ```sh
   rm -rf /path/to/node0/pd/rocksdb_old
   ```
   A second prune refuses to run while `rocksdb_old` exists.

Options: `--chunk-size N` (default 100000) trades memory for proof work;
`--delete-old-db` removes `rocksdb_old` automatically instead of keeping it.

If the prune is interrupted, nothing is lost. Both `pd migrate prune` and
`pd start` detect an interrupted directory swap and print the exact command to
restore the database.

If you run the prune outside the systemd unit, raise the open-file limit first
(`ulimit -n 65536`).

#### What is not pruned, on purpose

* CometBFT's block store (about 65 GB on mainnet). `pd` still tells CometBFT to
  retain every block. `pd` has no state sync, so new nodes join by replaying
  blocks from peers; if every node pruned its block store no new node could
  ever join. Block retention will be enabled once state sync exists.
* `pd`'s per-height transaction and compact block data, which wallets need to
  sync. RPC operators must never prune these.

Expect a pruned validator to use about 90 GB in total. See `docs/pruning.md`
for the full policy.

### Also in this release

* `pcli`: honor `--source` when selecting positions in `close-all` and
  `withdraw-all` (from 2.0.7).
* `cnidarium` 0.83.1, carried on `rotkonetworks/cnidarium`, adds the verified
  pruning API. No existing storage or proof code path is modified.
