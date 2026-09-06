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

**The node is fully offline while the prune runs.** Both `pd` and CometBFT
must be stopped; the prune needs exclusive access to the database. Expect
about an hour on NVMe for a mainnet store, longer on slower disks. The node
resumes at the same height afterwards and catches up from peers.

**Coordinate with other validators and do not prune at the same time.** If
more than one third of voting power is offline, the chain stops producing
blocks. Before you start, check that no other validator is pruning and that
the network has comfortably more than two thirds of stake online without you.

**If your validator alone holds more than one third of voting power, do not
prune at all.** Stopping it halts the chain for the duration. Wait until stake
is spread further, or reduce your share first.

1. Install the `pd` 2.0.8 binary (see below). It is a drop-in replacement.
2. Make sure at least 35 GB is free next to the `pd` data directory.
3. Stop both services. CometBFT exits when its ABCI connection to `pd` goes
   away, and under systemd it would otherwise crash-loop for the duration.
   ```sh
   sudo systemctl stop cometbft penumbra
   ```
4. Run the prune as the user that owns the `pd` data directory, with a raised
   open-file limit. `--home` is the `pd` home, the directory that contains
   `rocksdb`. Always pass it explicitly.
   ```sh
   sudo -u penumbra bash -c 'ulimit -n 65536; pd migrate --home /path/to/node0/pd prune'
   ```
   The log ends with `JMT pruning complete root_hash=...`. That root hash must
   match the one printed at the start in `starting JMT pruning`.
5. Start `pd` first, then CometBFT, and confirm the node is signing again.
   ```sh
   sudo systemctl start penumbra
   sudo systemctl start cometbft
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

#### Who should prune

* **Validators**: yes. Consensus only reads the latest state version.
* **RPC and archive nodes**: preferably not. Pruning removes every historical
  version of the state tree, so a pruned node can only serve state proofs from
  the prune point onwards. IBC relayers query proofs at specific recent heights
  and explorers may query state at a given height; point them at an unpruned
  node. Rotko keeps `penumbra.rotko.net` unpruned for this reason. If you do
  prune an RPC node, expect relayer queries to fail for the first minutes after
  the prune, until new versions accumulate again.

#### What is not pruned, on purpose

* CometBFT's block store (about 65 GB on mainnet). `pd` still tells CometBFT to
  retain every block. `pd` has no state sync, so new nodes join by replaying
  blocks from peers; if every node pruned its block store no new node could
  ever join. Block retention will be enabled once state sync exists.
* `pd`'s per-height transaction and compact block data, which wallets need to
  sync. RPC operators must never prune these.

Expect a pruned validator to use about 90 GB in total. See `docs/pruning.md`
for the full policy.

### Security: dependency advisories

`cargo audit` against the 2.0.7 lockfile reported 26 RustSec advisories. The
ones fixable without breaking changes on the Rust 1.83 toolchain this series
pins are patched in this release:

* `h2` 0.4.7 to 0.4.19 (RUSTSEC-2026-0258, unbounded empty DATA frames). This is
  the HTTP/2 implementation under `pd`'s public gRPC endpoint, so it is the one
  RPC operators should care about.
* `bytes` 1.9.0 to 1.12.1 (RUSTSEC-2026-0007, integer overflow in `reserve`).
* `openssl` 0.10.64 to 0.10.81 (RUSTSEC-2024-0357, 2025-0004, 2025-0022).
* `ring` 0.17.8 to 0.17.14 (RUSTSEC-2025-0009).
* `tar` 0.4.41 to 0.4.46 (RUSTSEC-2026-0067, 2026-0068).
* `aws-lc-sys` 0.25.0 to 0.41.0 (RUSTSEC-2026-0045 to 0048); only used by the
  optional `--grpc-auto-https` TLS termination.
* `crossbeam-epoch` 0.9.18 to 0.9.21, `tracing-subscriber` 0.3.18 to 0.3.20,
  `rustls` 0.23.21 to 0.23.23.

Still open, all requiring either a semver-breaking upgrade or a newer Rust
toolchain than 1.83, and tracked for the next release: `rustls-webpki` 0.101
and 0.102 (only reachable through `--grpc-auto-https`), `h2` 0.3 (legacy
`hyper` 0.14 path), `time` 0.3.44 (0.3.45+ needs cargo 1.85), `idna` 0.5,
`tracing-subscriber` 0.2 (via `ledger-lib`), and `rsa` 0.9 (no upstream fix;
not linked into `pd`).

CometBFT: run `v0.37.18` or later. It fixes CSA-2026-001 (critical) and
ASA-2025-003. Note the `0.37.18` binary reports itself as `0.37.16`.

### Also in this release

* `pcli`: honor `--source` when selecting positions in `close-all` and
  `withdraw-all` (from 2.0.7).
* `cnidarium` 0.83.1, carried on `rotkonetworks/cnidarium`, adds the verified
  pruning API. No existing storage or proof code path is modified.
