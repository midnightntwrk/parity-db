// Copyright 2026 Parity Technologies (UK) Ltd.
// This file is dual-licensed as Apache-2.0 or MIT.

//! Multi-column "substrate-shape" stress benchmark.
//!
//! The original `stress` benchmark hits a single column with reads and writes.
//! That's a worst-case for any optimisation that wins by sharding work across
//! columns (per-table LogOverlays flag, dashmap, etc.) — those optimisations
//! degenerate to no-ops on a single-column workload because all readers and
//! writers contend on the same shard.
//!
//! This bench mirrors midnight-node's column layout (`node/src/backend/
//! custom_parity_db.rs`): 13 polkadot columns, with STATE (1) and TRANSACTION
//! (11) configured ref-counted + uniform + preimage, and STATE / HEADER (4) /
//! BODY (5) / BODY_INDEX (12) / TRANSACTION / JUSTIFICATIONS (6) LZ4
//! compressed. The driver simulates substrate's block-import path:
//!
//!   - Each block writes to HEADER, BODY, JUSTIFICATIONS, TRANSACTION (×N),
//!     BODY_INDEX (×N), and STATE (×M).
//!   - RPC reader threads run concurrently, hammering STATE with random keys
//!     drawn from the already-imported range.
//!
//! This exercises a realistic spread of column activity: writer-active
//! columns (BODY, TX, etc.) AND read-active columns (STATE) at the same time.
//! Optimisations that only win when reader-column != writer-column should
//! show up here even though they didn't in the single-column bench.

use crate::Shared;
use parity_db::{ColumnOptions, CompressionType, Db};
use rand::{RngCore, SeedableRng};
use std::{
	sync::{
		atomic::{AtomicBool, AtomicUsize, Ordering},
		Arc,
	},
	thread,
	time::Instant,
};

// Column ids — must match midnight-node/node/src/backend/custom_parity_db.rs
const META: u8 = 0;
const STATE: u8 = 1;
const STATE_META: u8 = 2;
const KEY_LOOKUP: u8 = 3;
const HEADER: u8 = 4;
const BODY: u8 = 5;
const JUSTIFICATIONS: u8 = 6;
const AUX: u8 = 8;
const OFFCHAIN: u8 = 9;
const TRANSACTION: u8 = 11;
const BODY_INDEX: u8 = 12;
const NUM_COLUMNS_POLKADOT: u8 = 13;

#[allow(dead_code)]
const _USED: &[u8] = &[META, STATE_META, KEY_LOOKUP, AUX, OFFCHAIN]; // keep ids reachable

#[derive(Debug, clap::Parser)]
pub struct SubstrateStress {
	#[clap(flatten)]
	pub shared: Shared,

	/// Number of RPC-style reader threads. Each issues random STATE reads
	/// against the already-imported range.
	#[clap(long, default_value_t = 8)]
	pub readers: usize,

	/// Number of block-import writer threads.
	#[clap(long, default_value_t = 1)]
	pub writers: usize,

	/// Total blocks to import (split across writer threads).
	#[clap(long, default_value_t = 10_000)]
	pub blocks: usize,

	/// State entries written per block (ref-counted column 1, 32-byte uniform keys).
	#[clap(long, default_value_t = 50)]
	pub state_per_block: usize,

	/// Transactions per block (ref-counted column 11, 32-byte uniform keys).
	#[clap(long, default_value_t = 50)]
	pub tx_per_block: usize,

	/// Body-index entries per block (column 12).
	#[clap(long, default_value_t = 50)]
	pub body_index_per_block: usize,

	/// Random seed.
	#[clap(long, default_value_t = 1)]
	pub seed: u64,
}

pub fn run(args: SubstrateStress, base_path: std::path::PathBuf) -> Result<(), String> {
	let mut path = base_path;
	path.push("test_db_substrate_stress");
	if path.exists() {
		std::fs::remove_dir_all(&path).map_err(|e| format!("clear db: {e:?}"))?;
	}
	std::fs::create_dir_all(&path).map_err(|e| format!("mkdir: {e:?}"))?;

	let mut options = parity_db::Options::with_columns(&path, NUM_COLUMNS_POLKADOT);
	for &c in &[STATE, HEADER, BODY, BODY_INDEX, TRANSACTION, JUSTIFICATIONS] {
		options.columns[c as usize].compression = CompressionType::Lz4;
	}
	for &c in &[STATE, TRANSACTION] {
		let col: &mut ColumnOptions = &mut options.columns[c as usize];
		col.ref_counted = true;
		col.preimage = true;
		col.uniform = true;
	}

	let db = Arc::new(Db::open_or_create(&options).map_err(|e| format!("open db: {e:?}"))?);

	let shutdown = Arc::new(AtomicBool::new(false));
	let blocks_done = Arc::new(AtomicUsize::new(0));
	let state_writes_done = Arc::new(AtomicUsize::new(0));
	let queries_hit = Arc::new(AtomicUsize::new(0));
	let queries_miss = Arc::new(AtomicUsize::new(0));

	let start = Instant::now();
	let mut writer_handles = Vec::new();
	let blocks_per_writer = args.blocks / args.writers.max(1);

	for w in 0..args.writers {
		let db = db.clone();
		let blocks_done = blocks_done.clone();
		let state_writes_done = state_writes_done.clone();
		let writer_seed = args.seed.wrapping_add(0x100 + w as u64);
		let state_per = args.state_per_block;
		let tx_per = args.tx_per_block;
		let bi_per = args.body_index_per_block;
		writer_handles.push(
			thread::Builder::new()
				.name(format!("import-{w}"))
				.spawn(move || {
					let mut rng = rand::rngs::SmallRng::seed_from_u64(writer_seed);
					let writer_offset = (w as u64) * 1_000_000_000;
					for block in 0..blocks_per_writer {
						let block_id = writer_offset + block as u64;
						let mut commit = Vec::with_capacity(3 + tx_per + bi_per + state_per);

						// One header, body, justification per block.
						commit.push((
							HEADER,
							pack_key(b"hdr", block_id),
							Some(rand_bytes(&mut rng, 256)),
						));
						commit.push((
							BODY,
							pack_key(b"bdy", block_id),
							Some(rand_bytes(&mut rng, 50_000)),
						));
						commit.push((
							JUSTIFICATIONS,
							pack_key(b"jus", block_id),
							Some(rand_bytes(&mut rng, 4_000)),
						));

						// Transactions: ref-counted column, 32-byte uniform keys.
						for t in 0..tx_per {
							commit.push((
								TRANSACTION,
								blake_key(block_id, t as u64),
								Some(rand_bytes(&mut rng, 300)),
							));
						}

						// Body-index: lookup table from tx hash to body offset.
						for t in 0..bi_per {
							commit.push((
								BODY_INDEX,
								pack_key2(b"bi", block_id, t as u64),
								Some(rand_bytes(&mut rng, 64)),
							));
						}

						// State: ref-counted column, 32-byte uniform keys, small values.
						for s in 0..state_per {
							commit.push((
								STATE,
								blake_key(block_id, 1_000_000 + s as u64),
								Some(rand_bytes(&mut rng, 100)),
							));
						}

						db.commit(commit).expect("commit");
						blocks_done.fetch_add(1, Ordering::Relaxed);
						state_writes_done.fetch_add(state_per, Ordering::Relaxed);
					}
				})
				.unwrap(),
		);
	}

	let mut reader_handles = Vec::new();
	for r in 0..args.readers {
		let db = db.clone();
		let shutdown = shutdown.clone();
		let blocks_done = blocks_done.clone();
		let queries_hit = queries_hit.clone();
		let queries_miss = queries_miss.clone();
		let reader_seed = args.seed.wrapping_add(r as u64);
		let state_per = args.state_per_block as u64;
		let writers = args.writers;
		reader_handles.push(
			thread::Builder::new()
				.name(format!("rpc-{r}"))
				.spawn(move || {
					let mut rng = rand::rngs::SmallRng::seed_from_u64(reader_seed);
					while !shutdown.load(Ordering::Relaxed) {
						let imported = blocks_done.load(Ordering::Relaxed) as u64;
						if imported == 0 {
							continue;
						}
						// Pick a random already-imported block from any writer.
						let block_in_writer = rng.next_u64() % imported.max(1);
						let writer = (rng.next_u64() % writers as u64) * 1_000_000_000;
						let state_idx = 1_000_000 + rng.next_u64() % state_per;
						let key = blake_key(writer + block_in_writer, state_idx);
						match db.get(STATE, &key).expect("get") {
							Some(_) => queries_hit.fetch_add(1, Ordering::Relaxed),
							None => queries_miss.fetch_add(1, Ordering::Relaxed),
						};
					}
				})
				.unwrap(),
		);
	}

	for h in writer_handles {
		h.join().unwrap();
	}
	let write_secs = start.elapsed().as_secs_f64();
	let blocks = blocks_done.load(Ordering::Relaxed);
	let bps = blocks as f64 / write_secs;
	let hits_during_write = queries_hit.load(Ordering::Relaxed);
	let misses_during_write = queries_miss.load(Ordering::Relaxed);
	let qps_concurrent = (hits_during_write + misses_during_write) as f64 / write_secs;
	println!(
		"WRITE_PHASE blocks={blocks} secs={write_secs:.3} bps={bps:.2} \
		 concurrent_qps={qps_concurrent:.0} hits={hits_during_write} misses={misses_during_write}"
	);

	// Final read pass: stop writers, run readers full-tilt for a fixed query budget.
	queries_hit.store(0, Ordering::Relaxed);
	queries_miss.store(0, Ordering::Relaxed);
	let read_start = Instant::now();
	let target_queries = (args.readers * 1_000_000) as u64;
	while (queries_hit.load(Ordering::Relaxed) + queries_miss.load(Ordering::Relaxed)) <
		target_queries as usize
	{
		std::thread::sleep(std::time::Duration::from_millis(50));
	}
	shutdown.store(true, Ordering::Relaxed);
	for h in reader_handles {
		h.join().unwrap();
	}
	let read_secs = read_start.elapsed().as_secs_f64();
	let final_hits = queries_hit.load(Ordering::Relaxed);
	let final_misses = queries_miss.load(Ordering::Relaxed);
	let final_qps = (final_hits + final_misses) as f64 / read_secs;
	println!(
		"READ_PHASE secs={read_secs:.3} qps={final_qps:.0} hits={final_hits} misses={final_misses}"
	);

	Ok(())
}

fn pack_key(prefix: &[u8], block: u64) -> Vec<u8> {
	let mut k = Vec::with_capacity(prefix.len() + 8);
	k.extend_from_slice(prefix);
	k.extend_from_slice(&block.to_be_bytes());
	k
}

fn pack_key2(prefix: &[u8], a: u64, b: u64) -> Vec<u8> {
	let mut k = Vec::with_capacity(prefix.len() + 16);
	k.extend_from_slice(prefix);
	k.extend_from_slice(&a.to_be_bytes());
	k.extend_from_slice(&b.to_be_bytes());
	k
}

/// 32-byte hash of (a, b) — mirrors the uniform-key contract for STATE / TX.
fn blake_key(a: u64, b: u64) -> Vec<u8> {
	use blake2::{
		digest::{typenum::U32, FixedOutput, Update},
		Blake2b,
	};
	let mut hasher = Blake2b::<U32>::default();
	hasher.update(&a.to_le_bytes());
	hasher.update(&b.to_le_bytes());
	hasher.finalize_fixed().to_vec()
}

fn rand_bytes(rng: &mut rand::rngs::SmallRng, len: usize) -> Vec<u8> {
	let mut v = vec![0u8; len];
	rng.fill_bytes(&mut v);
	v
}
