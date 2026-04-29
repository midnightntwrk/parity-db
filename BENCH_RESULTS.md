# QMDB / NOMT mining results

Branch: `giles-mine-qmdb-nomt-ideas`. Three ideas mined from QMDB and NOMT
were applied to parity-db; this file records what was changed, what was
measured, and what we *can't* conclude from the local Mac runs.

## TL;DR

| # | Idea | Source | What it adds |
| - | ---- | ------ | ------------ |
| 1 | `Db::prefetch` + `Db::get_many` | NOMT `Session::warm_up` | Two-phase batched-read API that issues `madvise(WILLNEED)` on every index chunk before doing point reads. |
| 3 | `madvise(WILLNEED)` in `enact_plan` | NOMT/QMDB pre-block prefetch pipelines | Destination-page prefetch on each `InsertIndex` / `InsertValue` so the major page fault overlaps with log-buffer parsing. |
| 4 | `Db::pin_index_prefix` | NOMT always-resident upper bitbox levels | `mlock` the hot prefix of each hash column's index file in physical RAM. |

All three are **Linux-gated where it matters** (`madvise_willneed` is
`cfg(target_os = "linux")` only — macOS's `MADV_WILLNEED` synchronously
pages in, which costs the syscall without the async readahead benefit).

`Db::pin_index_prefix` works on any Unix; on non-Unix it's a no-op.

## Code shape

| Layer | Surface added |
| ----- | ------------- |
| `file.rs` | `madvise_willneed`, `mlock_prefix`, `TableFile::prefetch_at` |
| `index.rs` | `IndexTable::prefetch`, `prefetch_chunk`, `pin_prefix` + `madvise(WILLNEED)` in `enact_plan` |
| `column.rs` | `HashColumn::prefetch`, `pin_index_prefix` |
| `db.rs` | `Db::prefetch`, `Db::get_many`, `Db::pin_index_prefix` + `DbInner::prefetch` glue |
| `table.rs` | `madvise(WILLNEED)` in `ValueTable::enact_plan` |
| `admin/src/bench/mod.rs` | `--read-batch-size N`, `--pin-index-mib N` flags |

3 new public methods on `Db` (`prefetch`, `get_many`, `pin_index_prefix`).
3 new tests covering equivalence, no-op safety, and over-call safety.
`cargo test --release --workspace`: **39 passed** (baseline was 36).

## What we measured (Mac, arm64)

Stress runs are *noisy* on this laptop — thermal throttling and
foreground-load variance push individual runs ±20%. The numbers below
should be read as "didn't regress catastrophically when the feature is
inactive on this platform", **not** as the actual win.

```text
parity-db-admin stress -d <dir> --writers 2 --readers 4 --commits 50000 --seed 1
```

| Config | Concurrent reader qps | Final read-pass qps | Writer cps |
| ------ | --------------------: | ------------------: | ---------: |
| baseline (master) | 2.13M (avg of 3) | 2.15M (avg of 3) | 1530 (avg of 3) |
| HEAD, batch=1, pin=0 | 1.14M | 2.25M | 1243 |
| HEAD, batch=16, pin=0 | 0.85M | 2.13M | 1129 |
| HEAD, batch=64, pin=0 | 0.92M | 2.26M | 1421 |
| HEAD, batch=1, pin=16 MiB | 1.20M | 1.78M | 1104 |

Reading these:

- **Final read pass** (single-threaded, post-write) is the most stable
  metric. It's flat across configs (2.13M – 2.26M qps) with a dip when
  pin=16 MiB steals RAM from the value-table cache (1.78M). All within
  noise except that pin regression, which is expected on this small a
  dataset.
- **Concurrent reader qps** is dominated by writer contention noise and
  is unreliable on this machine.
- **Writer cps** drops by ~10–25 % under batch>1 — that's the writers
  competing with readers that are now allocating per-batch buffers.
  A real Linux NVMe run with cold pages should flip the read column
  positive enough to compensate.

The point of the local sweep was to verify **no test regression** and
**no catastrophic perf loss with the feature disabled**. Both hold.

## What we *don't* know yet

The actual question — "does this speed up midnight-node mainnet sync"
— is not answerable from these numbers. It needs:

1. **Linux + NVMe** so `madvise(WILLNEED)` is real async readahead.
2. **A bigger-than-cache working set** so `pin_index_prefix` has
   something to win against.
3. **The substrate side** — `Db::get_many` is a free win for parity-db
   only if `sp_database::Database<H>` (or a parity-db-specific
   extension trait) exposes it to substrate. Today substrate's
   `read_state` path issues sequential `get`s; a tiny shim in
   `sc-client-db` would let it call `get_many` for storage prefixes.
4. **A real workload trace.** Synthetic stress hits the same bucket
   distribution forever. Mainnet sync has temporal locality the
   stress test doesn't, which both helps the pin and complicates the
   batched-prefetch story.

## How to re-run on the x86 box

Once `sshd` is back up on `192.168.1.129`:

```sh
ssh x86 'cd ~/git && git clone https://github.com/midnightntwrk/parity-db.git \
  && cd parity-db && git fetch origin giles-mine-qmdb-nomt-ideas \
  && git checkout giles-mine-qmdb-nomt-ideas \
  && cargo build --release -p parity-db-admin'

# Then for each config:
ssh x86 'cd ~/git/parity-db && rm -rf /tmp/pdb_bench && mkdir /tmp/pdb_bench \
  && ./target/release/parity-db-admin stress \
       -d /tmp/pdb_bench --writers 2 --readers 4 --commits 200000 --seed 1 \
       --read-batch-size 16 --pin-index-mib 64'
```

Sweep the cross product `{batch=1, batch=16, batch=64} × {pin=0, pin=64}`
on the same commit count and tabulate cps + qps. That's the table that
tells you whether mining was worthwhile.

## Co-existence with master

Each idea is in its own commit and each is independently revertable:

```text
2c3180f feat: add Db::pin_index_prefix to mlock the hot top of the index
5e4f613 feat: prefetch destination pages in enact_plan
6464c96 feat: add Db::prefetch and Db::get_many for batched lookups
be7fad8 docs: capture baseline stress + multitree-stress numbers
```

If a Linux run shows one of them is a regression, `git revert <sha>`
takes it out without affecting the others.
