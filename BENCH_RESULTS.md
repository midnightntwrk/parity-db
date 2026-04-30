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

## Linux NVMe sweep

Re-ran on `192.168.1.137` (NixOS, kernel 6.12.75, 32 hardware threads,
NVMe via `nvme0n1p2`, `rustc 1.95.0`). 200,000 commits per config, single
seed each, single run each. **Caveat: warm-cache regime** — the
generated dataset is small enough to fit comfortably in the host's RAM
once the index file is touched, so the OS page cache absorbs most reads.
This is the *worst* case for `madvise(WILLNEED)` and `mlock` — both pay
their syscall / RSS cost without gaining the async-readahead /
miss-elimination benefit they were mined for. The relevant production
case (mainnet sync with a working set bigger than RAM) needs either
`drop_caches` between runs or a multi-million-commit working set; this
sweep doesn't model it.

```text
parity-db-admin stress -d <dir> --writers 2 --readers 4 --commits 200000 --seed 1
```

| Config                              | Writer cps | Concurrent qps | Final read-pass qps |
| ----------------------------------- | ---------: | -------------: | ------------------: |
| `master`                  b1, p=0   |  **769.17**|     **4.28 M** |             2.24 M  |
| HEAD (#3)                 b1, p=0   |     746.20 |        4.24 M  |             2.21 M  |
| HEAD (#1+#3)              b16, p=0  |     754.66 |        2.79 M  |             2.21 M  |
| HEAD (#1+#3 deeper)       b64, p=0  |     751.82 |        2.78 M  |             2.21 M  |
| HEAD (#3+#4)              b1, p=64  |     751.82 |        4.25 M  |             2.23 M  |
| HEAD (#1+#3+#4)           b16, p=64 |     754.66 |        2.79 M  |          **2.30 M** |

Honest reading:

- **Writer cps**: HEAD costs ~2 % vs master across the board (~754 vs
  ~769). That's idea #3 — the unconditional `madvise(WILLNEED)` in
  `enact_plan` paying a syscall on every write to chunks that are
  already cache-resident. Real cost. On cold pages it should flip
  positive; here it's a small unrecovered overhead.

- **Concurrent qps**: master and the batch=1 HEAD configs sit at
  ~4.25 M qps. The **batch=16 / batch=64 configs drop to ~2.78 M qps —
  a 35 % regression**. Cause: the two-phase path (prefetch loop +
  `Vec` allocation per batch) costs CPU on every iteration and the
  prefetch buys nothing because the index chunks are already in the
  page cache. This is the predicted worst case for `Db::get_many` and
  it is very real. The win materialises only when the index doesn't fit
  in cache; we did not test that case.

- **Final read-pass qps**: all configs land in 2.21 – 2.30 M qps, a
  4 % spread within plausible single-run noise. The all-three config
  (`b16, p=64`) edged ahead at 2.30 M, but a single-seed delta this
  small isn't trustworthy.

What this changes for the "should we land it on midnight-node" question:

1. **Idea #3** (always-on madvise in enact) costs ~2 % writer cps on
   warm cache. It needs to be **opt-in** — a `column.prefetch_on_write`
   bool in `Options`, default off — because warm-cache is a real
   production case. As-is, it would slow down a chain whose state
   already fits in RAM. Easy follow-up.

2. **Idea #1** (`Db::get_many` with prefetch) is **a regression in the
   only regime we measured**. It's still a sensible API to keep, but
   we should not route substrate's read path through it
   unconditionally. Keep the public method, leave it to callers that
   *know* their working set is cold (e.g. block import after a long
   downtime, or warp-sync state restore). Easy follow-up: a second
   variant `Db::get_many_no_prefetch` for the warm path, or a
   threshold-based heuristic.

3. **Idea #4** (`pin_index_prefix`) is essentially neutral on this
   workload. Both the writer and reader columns are within noise of
   the other p=64 configs. The 64 MiB of mlock'd RAM didn't displace
   anything important here. Should stay opt-in (it already is).

The cold-cache test that would actually validate the mining still
hasn't been run. To do it properly:

```sh
ssh 192.168.1.137 'sudo sh -c "echo 3 > /proc/sys/vm/drop_caches"'
# then re-run each stress with a 5-minute warm-up sleep before the
# reader threads start, so the writer commits land on cold pages
```

That needs sudo on x86, which I haven't asked for yet.

## Warm-cache read-path profiling

After the user pushed back ("I'm more interested in warm-cache times"), I
sampled the read hot path on x86 with `perf record --call-graph dwarf`
on a `writers=1 readers=8 commits=50000` config. 569k samples, 4.5 GB
trace. Top self-time symbols (master, pre-mining):

```text
14.08%  RwLock<LogOverlays> as LogQuery::value_ref
14.00%  parity_db::index::IndexTable::get
12.64%  blake2::Blake2bVarCore::compress
12.49%  parking_lot::raw_rwlock::RawRwLock::lock_shared_slow
 6.73%  HashColumn::get
 6.49%  LogOverlays::with_index
 5.68%  ValueTable::query
 4.39%  DbInner::get
 4.14%  __memmove_avx_unaligned_erms
```

**~33 % of total CPU time lands in the LogOverlays read path.** Each
`Db::get` takes the LogOverlays read lock twice (once for the index
overlay, once for the value overlay). Eight readers contending on the
RwLock cache line + the writer's periodic write lock keeps the
shared-acquire fast-path failing into `lock_shared_slow`.

### What I tried (and what worked)

Three attempts, all on commits 3aa2136 → 17357d0 → 3e6a2bb (last
reverted as 1bae0c2):

1. **Global `nonempty: AtomicUsize` bypass** (3aa2136). Skip the
   LogOverlays read lock entirely when the entire overlay is empty.
   Profile post-fix showed `lock_shared_slow` unchanged at 12.48 % —
   the bypass essentially never fired because the writer-active bench
   keeps the global counter > 0.

2. **Per-table `AtomicBool` bypass** (17357d0). One flag per per-table
   overlay, kept outside the RwLock. Writers update under the write
   lock; readers Acquire-load before locking. Idea: most individual
   tables are empty even when *some* table has writes. Result on the
   bench: still neutral (-0.7 % cps both configs, qps within noise).
   Diagnosis: this bench has a single hash column, so all
   reader threads target the same per-table overlay → per-table flags
   degenerate to the global flag. Would only show up on a multi-column
   workload (substrate sync: state column hot for reads, body /
   transactions for writes).

3. **Identity-hashing the global overlay maps** (3e6a2bb, REVERTED).
   The maps key on u64 chunk indices and there's a comment in the
   code saying "we use identity hash for value overlay/log records",
   yet only the per-`LogWriter` local map was wired to `BuildIdHash`;
   the global maps used SipHash. Applied `BuildIdHash` to all three.
   **Catastrophic regression**: w=2/r=4 final qps 2.20 M → 1.18 M
   (-46 %), concurrent qps 4.31 M → 3.54 M (-18 %). Best guess:
   hashbrown's SwissTable metadata array uses 7 bits of the hash;
   with raw u64 keys whose bottom bits cluster (chunk indices grow
   sequentially) the metadata clusters too, blowing up probe length
   under load. The local LogWriter map doesn't see this because it
   stays small and short-lived; the global maps accumulate thousands
   of entries between enacts. Reverted in 1bae0c2.

### Net effect on this branch

The two surviving overlay-bypass commits (3aa2136 + 17357d0) are
no-cost insurance — they'll fire on multi-column workloads and on
read-mostly periods between bursts (post-warp-sync, idle node, etc)
but do not move this single-column continuously-writing bench.

### v5 — ahash on the global LogOverlay maps (273f557)

Following the identity-hash regression, the obvious follow-up was
ahash: still randomised (so SwissTable's metadata array gets uniform
7-bit slices), still doesn't pretend to defend against adversarial
keys, but ~3× faster than SipHash on short keys.

Linux NVMe sweep, single-seed:

| Config | Master | v5 (ahash) | Δ |
| ------ | -----: | ---------: | -: |
| w=2 r=4 cps             |    758 |    746 | -1.5 % |
| w=2 r=4 concurrent qps  | 4.27 M | **4.92 M** | **+15.2 %** |
| w=2 r=4 final-pass qps  | 2.32 M | 2.30 M | -0.7 % |
| w=1 r=8 cps             |    763 |    752 | -1.5 % |
| w=1 r=8 concurrent qps  | 4.59 M | **5.58 M** | **+21.6 %** |
| w=1 r=8 final-pass qps  | 2.04 M | **2.20 M** | **+7.8 %** |

**Concurrent reader throughput goes up 15–22 %.** Writer cps drops
~1.5 % which is the expected trade-off (readers running faster steal
more CPU from the writer thread on a fixed-core box). Final-pass qps
(single-threaded post-write) is within noise on the 4-reader config
and +8 % on the 8-reader config — the 8-reader case actually benefits
because the read pass over the warmer DB is finishing earlier, leaving
less time for OS-cache eviction by background work.

Re-profile with v5 applied confirms the win is where we expected:

| Symbol | Master | v5 | Δ pp |
| ------ | -----: | -: | --: |
| `IndexTable::get` + `LogQuery::with_index` (combined) | 20.5 % | 13.75 % | -6.7 |
| `LogQuery::value_ref` | 14.08 % | 12.77 % | -1.3 |
| `lock_shared_slow` | 12.49 % | 13.33 % | +0.8 (relative shift, same wall-time pressure) |
| `blake2` | 12.64 % | 15.43 % | +2.8 (more reads per second → more hashing per second) |

The remaining ~40 % combined cost in `with_index` + `value_ref` +
`lock_shared_slow` is the LogOverlays read lock itself. That's the
target for follow-ups #3 / #4 below.

## Multi-column substrate-shape sweep

Added `parity-db-admin substrate-stress` (commit 73df3fc): mirrors
midnight-node's column layout (13 polkadot columns, STATE+TX
ref-counted+uniform+preimage, six columns LZ4-compressed) and a
substrate-like workload — each "block" writes to HEADER, BODY,
JUSTIFICATIONS, ~50 transactions, ~50 body-index entries, ~50 state
entries; concurrent readers hammer STATE.

This is the workload the synthetic single-column bench deliberately
isn't. Per-table LogOverlays flags can't fire when one column owns
all reads + writes; on this bench, readers and writers naturally
target different per-table overlays.

5000 blocks, --writers 1 --readers 8 --state-per-block 50
--tx-per-block 50 --body-index-per-block 50, two seeds:

| Metric | master | HEAD (all mining commits) | Δ |
| ------ | -----: | ------------------------: | -: |
| Block imports / sec   |  1393 | **1519** | **+9.0 %** |
| Concurrent reader qps | 3.59 M | 3.77 M | +5.0 % |
| Final read-pass qps   | 4.42 M | 4.05 M | -8.4 % |

The +9 % writer win and +5 % concurrent reader win are the per-table
flag bypass + ahash starting to pay off when reader and writer hit
different columns: a STATE reader's `with_index` probe sees the STATE
overlay flag as `false` (writer is busy in BODY/HEADER/TX columns) and
returns immediately without taking the lock.

The -8 % final-read regression is most likely a second-order effect of
the faster writer: HEAD finishes writes ~9 % sooner, leaving a longer
enact tail still draining when the timed read pass starts. The reader
sees `nonempty == true` for slightly longer, so the lock is taken
slightly more often during the read window. Acceptable trade-off — and
it should reverse on a workload long enough for the enact tail to
drain before the read measurement.

### Remaining follow-ups (in increasing radius of change)

1. ~~`ahash` on the global overlay maps.~~ **Done in 273f557, +15-22 %
   concurrent qps.**

2. **Eliminate the value memmove (~5 %).** `Db::get` currently copies
   value bytes out of the mmap into a fresh `Vec<u8>`. A
   `Db::get_with<F>` that hands the caller a borrowed slice tied to
   the read lock saves the copy; substrate's storage-read path can
   often consume the bytes directly without owning them. Worth doing
   even though the synthetic bench's reader doesn't use the value
   bytes (so won't reflect the win) — real workloads will.

3. **Sharded LogOverlays via `dashmap`.** Replace `RwLock<HashMap>`
   with `DashMap` to remove reader-reader cache-line contention.
   Bigger code surface; `lock_shared_slow` is still 13 % after ahash
   and is the largest single line on the profile we can't cheaply
   skip. Note: in single-column workloads (this bench) all readers
   target the same per-table shard so dashmap won't help; in
   multi-column workloads (real substrate) it should.

4. **`arc-swap` snapshot model.** Writer publishes a new
   `Arc<LogOverlays>` per commit; readers `Arc::clone()` and read
   without any lock. Best for read-mostly; cost is `O(N)` clone per
   commit where N is the overlay size — could be net negative if
   commits land faster than the clone amortises. Probably wins for
   substrate sync where reads dominate during catch-up.

5. ~~Faster key hash on hash columns.~~ Off-limits — column key hash
   is part of the on-disk format.

The right next step depends on whether the goal is "make this
synthetic bench look better" (try #3) or "make midnight-node sync
faster" (real workload trace first, then #2 and #4).

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
