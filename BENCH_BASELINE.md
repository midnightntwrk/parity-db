# Baseline benchmark numbers

Captured before any QMDB/NOMT mining changes. Branch: `giles-mine-qmdb-nomt-ideas`,
master commit `e751668`.

**Caveat:** these numbers were captured on an Apple-silicon Mac (no `io_uring`, no
NVMe with the same write-amp characteristics as the x86 NixOS box). They are
intended as a sanity baseline so we can detect regressions during mining; the
authoritative comparison numbers must be re-run on the Linux NVMe box.

## Hardware / OS

- macOS / Apple-silicon (arm64)
- Storage: APFS, internal SSD
- Filesystem cache effects not isolated

## Stress test (`parity-db-admin stress`)

Command:

```text
parity-db-admin stress -d <dir> --writers 2 --readers 4 --commits 50000 --seed <N>
```

| Run | Writes (cps) | Reads (qps) |
| --- | -----------: | ----------: |
| 1   |        1,717 |   2,207,280 |
| 2   |        ~1580 |   2,079,815 |
| 3   |        ~1295 |   2,154,061 |

(Run 3 was the slowest of the three, suggesting some thermal/foreground-load
variability on this machine — the x86 box should be more stable.)

## Multitree stress test (`parity-db-admin multi-tree-stress`)

Command:

```text
parity-db-admin multi-tree-stress -d <dir> --writers 2 --readers 4 --commits 1000 --seed 1
```

| Metric          | Value |
| --------------- | ----: |
| Tree commits/s  |   282 |
| Total queries   | 1,050,281 |

## Test suite

`cargo test --release --workspace`: 36 tests passing.
