// Copyright 2015-2020 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use parking_lot::RwLock;
use crate::{
	error::{Error, Result},
	table::{TableId as ValueTableId, ValueTable, Key, Value},
	log::{Log, LogOverlays, LogReader, LogWriter, LogAction},
	display::hex,
	index::{IndexTable, TableId as IndexTableId, PlanOutcome, Address},
	options::{Options, ColumnOptions, Metadata},
	stats::ColumnStats,
	db::check::CheckDisplay,
};
use crate::compress::Compress;

const START_BITS: u8 = 16;
const MAX_REBALANCE_BATCH: usize = 8192;

pub type ColId = u8;
pub type Salt = [u8; 32];

struct Tables {
	index: IndexTable,
	value: Vec<ValueTable>,
}

struct Reindex {
	queue: VecDeque<IndexTable>,
	progress: AtomicU64,
}

pub struct Column {
	tables: RwLock<Tables>,
	reindex: RwLock<Reindex>,
	path: std::path::PathBuf,
	preimage: bool,
	uniform_keys: bool,
	collect_stats: bool,
	ref_counted: bool,
	salt: Option<Salt>,
	stats: ColumnStats,
	compression: Compress,
	db_version: u32,
}

pub struct IterState {
	pub chunk_index: u64,
	pub key: Key,
	pub rc: u32,
	pub value: Vec<u8>,
}

enum IterStateOrCorrupted {
	Item(IterState),
	Corrupted(crate::index::Entry, Option<Error>),
}

impl Column {
	pub fn get(&self, key: &Key, log: &RwLock<LogOverlays>) -> Result<Option<Value>> {
		let tables = self.tables.read();
		if let Some((tier, value)) = self.get_in_index(key, &tables.index, &*tables, log)? {
			if self.collect_stats {
				self.stats.query_hit(tier);
			}
			return Ok(Some(value));
		}
		for r in &self.reindex.read().queue {
			if let Some((tier, value)) = self.get_in_index(key, &r, &*tables, log)? {
				if self.collect_stats {
					self.stats.query_hit(tier);
				}
				return Ok(Some(value));
			}
		}
		if self.collect_stats {
			self.stats.query_miss();
		}
		Ok(None)
	}

	pub fn get_size(&self, key: &Key, log: &RwLock<LogOverlays>) -> Result<Option<u32>> {
		self.get(key, log).map(|v| v.map(|v| v.len() as u32))
	}

	fn get_in_index(&self, key: &Key, index: &IndexTable, tables: &Tables, log: &RwLock<LogOverlays>) -> Result<Option<(u8, Value)>> {
		let (mut entry, mut sub_index) = index.get(key, 0, log);
		while !entry.is_empty() {
			let size_tier = entry.address(index.id.index_bits()).size_tier() as usize;
			match tables.value[size_tier].get(key, entry.address(index.id.index_bits()).offset(), log)? {
				Some((value, compressed)) => {
					let value = if compressed {
						self.decompress(&value)
					} else {
						value
					};
					return Ok(Some((size_tier as u8, value)));
				}
				None =>  {
					let (next_entry, next_index) = index.get(key, sub_index + 1, log);
					entry = next_entry;
					sub_index = next_index;
				}
			}
		}
		Ok(None)
	}

	/// Compress if needed and return the target tier to use.
	fn compress(&self, key: &Key, value: &[u8], tables: &Tables) -> (Option<Vec<u8>>, usize) {
		Self::compress_internal(&self.compression, key, value, tables)
	}

	fn compress_internal(compression: &Compress, key: &Key, value: &[u8], tables: &Tables) -> (Option<Vec<u8>>, usize) {
		let (len, result) = if value.len() > compression.treshold as usize {
			let cvalue = compression.compress(value);
			if cvalue.len() < value.len() {
				(cvalue.len(), Some(cvalue))
			} else {
				(value.len(), None)
			}
		} else {
			(value.len(), None)
		};
		let target_tier = tables.value.iter().position(|t| len <= t.value_size() as usize);
		let target_tier = match target_tier {
			Some(tier) => tier as usize,
			None => {
				log::trace!(target: "parity-db", "Using blob {}", hex(key));
				tables.value.len() - 1
			}
		};

		(result, target_tier)
	}

	fn decompress(&self, buf: &[u8]) -> Vec<u8> {
		self.compression.decompress(buf)
	}

	pub fn open(col: ColId, options: &Options, metadata: &Metadata) -> Result<Column> {
		let (index, reindexing, stats) = Self::open_index(&options.path, col)?;
		let collect_stats = options.stats;
		let path = &options.path;
		let arc_path = std::sync::Arc::new(path.clone());
		let options = &metadata.columns[col as usize];
		let db_version = metadata.version;
		let tables = Tables {
			index,
			value: (0.. options.sizes.len() + 1)
				.map(|i| Self::open_table(arc_path.clone(), col, i as u8, &options, db_version)).collect::<Result<_>>()?
		};

		Ok(Column {
			tables: RwLock::new(tables),
			reindex: RwLock::new(Reindex {
				queue: reindexing,
				progress: AtomicU64::new(0),
			}),
			path: path.into(),
			preimage: options.preimage,
			uniform_keys: options.uniform,
			ref_counted: options.ref_counted,
			collect_stats,
			salt: metadata.salt.clone(),
			stats,
			compression: Compress::new(options.compression, options.compression_treshold),
			db_version,
		})
	}

	pub fn hash(&self, key: &[u8]) -> Key {
		let mut k = Key::default();
		if self.uniform_keys {
			k.copy_from_slice(&key[0..32]);
		} else if let Some(salt) = &self.salt {
			k.copy_from_slice(blake2_rfc::blake2b::blake2b(32, &salt[..], &key).as_bytes());
		} else {
			k.copy_from_slice(blake2_rfc::blake2b::blake2b(32, &[], &key).as_bytes());
		}
		k
	}

	pub fn flush(&self) -> Result<()> {
		let tables = self.tables.read();
		tables.index.flush()?;
		for t in tables.value.iter() {
			t.flush()?;
		}
		Ok(())
	}

	fn open_index(path: &std::path::Path, col: ColId) -> Result<(IndexTable, VecDeque<IndexTable>, ColumnStats)> {
		let mut reindexing = VecDeque::new();
		let mut top = None;
		let mut stats = ColumnStats::empty();
		for bits in (START_BITS .. 65).rev() {
			let id = IndexTableId::new(col, bits);
			if let Some(table) = IndexTable::open_existing(path, id)? {
				if top.is_none() {
					stats = table.load_stats();
					top = Some(table);
				} else {
					reindexing.push_front(table);
				}
			}
		}
		let table = match top {
			Some(table) => table,
			None => IndexTable::create_new(path, IndexTableId::new(col, START_BITS)),
		};
		Ok((table, reindexing, stats))
	}

	fn open_table(
		path: std::sync::Arc<std::path::PathBuf>,
		col: ColId,
		tier: u8,
		options: &ColumnOptions,
		db_version: u32,
	) -> Result<ValueTable> {
		let id = ValueTableId::new(col, tier);
		let entry_size = options.sizes.get(tier as usize).cloned();
		ValueTable::open(path, id, entry_size, options, db_version)
	}

	fn trigger_reindex(
		tables: parking_lot::RwLockUpgradableReadGuard<Tables>,
		reindex: parking_lot::RwLockUpgradableReadGuard<Reindex>,
		path: &std::path::Path,
	) {
		let mut tables = parking_lot::RwLockUpgradableReadGuard::upgrade(tables);
		let mut reindex = parking_lot::RwLockUpgradableReadGuard::upgrade(reindex);
		log::info!(
			target: "parity-db",
			"Started reindex for {}",
			tables.index.id,
		);
		// Start reindex
		let new_index_id = IndexTableId::new(
			tables.index.id.col(),
			tables.index.id.index_bits() + 1
		);
		let new_table = IndexTable::create_new(path, new_index_id);
		let old_table = std::mem::replace(&mut tables.index, new_table);
		reindex.queue.push_back(old_table);
	}

	pub fn write_reindex_plan(&self, key: &Key, address: Address, log: &mut LogWriter) -> Result<PlanOutcome> {
		let tables = self.tables.upgradable_read();
		let reindex = self.reindex.upgradable_read();
		if Self::search_index(key, &tables.index, &*tables, log)?.is_some() {
			return Ok(PlanOutcome::Skipped);
		}
		match tables.index.write_insert_plan(key, address, None, log)? {
			PlanOutcome::NeedReindex => {
				log::debug!(target: "parity-db", "{}: Index chunk full {}", tables.index.id, hex(key));
				Self::trigger_reindex(tables, reindex, self.path.as_path());
				self.write_reindex_plan(key, address, log)?;
				return Ok(PlanOutcome::NeedReindex);
			}
			_ => {
				return Ok(PlanOutcome::Written);
			}
		}
	}

	fn search_index<'a>(
		key: &Key,
		index: &'a IndexTable,
		tables: &'a Tables,
		log: &LogWriter
	) -> Result<Option<(&'a IndexTable, usize, u8, Address)>> {
		let (mut existing_entry, mut sub_index) = index.get(key, 0, log);
		while !existing_entry.is_empty() {
			let existing_address = existing_entry.address(index.id.index_bits());
			let existing_tier = existing_address.size_tier();
			if tables.value[existing_tier as usize].has_key_at(existing_address.offset(), &key, log)? {
				return Ok(Some((&index, sub_index, existing_tier, existing_address)));
			}

			let (next_entry, next_index) = index.get(key, sub_index + 1, log);
			existing_entry = next_entry;
			sub_index = next_index;
		};
		Ok(None)
	}

	fn search_all_indexes<'a>(
		key: &Key,
		tables: &'a Tables,
		reindex: &'a Reindex,
		log: &LogWriter
	) -> Result<Option<(&'a IndexTable, usize, u8, Address)>> {
			if let Some(r) = Self::search_index(key, &tables.index, tables, log)? {
				return Ok(Some(r));
			}
			// Check old indexes
			// TODO: don't search if index precedes reindex progress
			for index in &reindex.queue {
				if let Some(r) = Self::search_index(key, index, tables, log)? {
					return Ok(Some(r));
				}
			}
			Ok(None)
	}

	pub fn write_plan(&self, key: &Key, value: &Option<Value>, log: &mut LogWriter) -> Result<PlanOutcome> {
		//TODO: return sub-chunk position in index.get
		let tables = self.tables.upgradable_read();
		let reindex = self.reindex.upgradable_read();
		let existing = Self::search_all_indexes(key, &*tables, &*reindex, log)?;
		if let &Some(ref val) = value {
			if let Some((table, sub_index, existing_tier, existing_address)) = existing {
				let existing_tier = existing_tier as usize;
				if self.ref_counted {
					log::trace!(target: "parity-db", "{}: Increment ref {}", tables.index.id, hex(key));
					tables.value[existing_tier].write_inc_ref(existing_address.offset(), log)?;
					return Ok(PlanOutcome::Written);
				}
				if self.preimage {
					// Replace is not supported
					return Ok(PlanOutcome::Skipped);
				}
				let (cval, target_tier) = self.compress(&key, &val, &*tables);
				let (cval, compressed) = cval.as_ref()
					.map(|cval| (cval.as_slice(), true))
					.unwrap_or((val.as_slice(), false));

				if self.collect_stats {
					let (cur_size, compressed) = tables.value[existing_tier].size(&key, existing_address.offset(), log)?
						.unwrap_or((0, false));
					if compressed {
						// This is very costly.
						let compressed = tables.value[existing_tier].get(&key, existing_address.offset(), log)?
							.expect("Same query as size").0;
						let uncompressed = self.decompress(compressed.as_slice());

						self.stats.replace_val(cur_size, uncompressed.len() as u32, val.len() as u32, cval.len() as u32);
					} else {
						self.stats.replace_val(cur_size, cur_size, val.len() as u32, cval.len() as u32);
					}
				}
				if existing_tier == target_tier {
					log::trace!(target: "parity-db", "{}: Replacing {}", tables.index.id, hex(key));
					tables.value[target_tier].write_replace_plan(existing_address.offset(), key, &cval, log, compressed)?;
					return Ok(PlanOutcome::Written);
				} else {
					log::trace!(target: "parity-db", "{}: Replacing in a new table {}", tables.index.id, hex(key));
					tables.value[existing_tier].write_remove_plan(existing_address.offset(), log)?;
					let new_offset = tables.value[target_tier].write_insert_plan(key, &cval, log, compressed)?;
					let new_address = Address::new(new_offset, target_tier as u8);
					// If it was found in an older index we just insert a new entry. Reindex won't overwrite it.
					let sub_index = if table.id == tables.index.id { Some(sub_index) } else { None };
					return tables.index.write_insert_plan(key, new_address, sub_index, log);
				}
			} else {
				let (cval, target_tier) = self.compress(&key, &val, &*tables);
				let (cval, compressed) = cval.as_ref()
					.map(|cval| (cval.as_slice(), true))
					.unwrap_or((val.as_slice(), false));

				log::trace!(target: "parity-db", "{}: Inserting new index {}, size = {}", tables.index.id, hex(key), cval.len());
				let offset = tables.value[target_tier].write_insert_plan(key, &cval, log, compressed)?;
				let address = Address::new(offset, target_tier as u8);
				match tables.index.write_insert_plan(key, address, None, log)? {
					PlanOutcome::NeedReindex => {
						log::debug!(target: "parity-db", "{}: Index chunk full {}", tables.index.id, hex(key));
						Self::trigger_reindex(tables, reindex, self.path.as_path());
						self.write_plan(key, value, log)?;
						return Ok(PlanOutcome::NeedReindex);
					}
					_ => {
						if self.collect_stats {
							self.stats.insert_val(val.len() as u32, cval.len() as u32);
						}
						return Ok(PlanOutcome::Written);
					}
				}
			}
		} else {
			if let Some((table, sub_index, existing_tier, existing_address)) = existing {
				// Deletion
				let existing_tier = existing_tier as usize;
				let cur_size = if self.collect_stats {
					let (cur_size, compressed) = tables.value[existing_tier].size(&key, existing_address.offset(), log)?
						.unwrap_or((0, false));
					Some(if compressed {
						// This is very costly.
						let compressed = tables.value[existing_tier].get(&key, existing_address.offset(), log)?
							.expect("Same query as size").0;
						let uncompressed = self.decompress(compressed.as_slice());

						(cur_size, uncompressed.len() as u32)
					} else {
						(cur_size, cur_size)
					})
				} else {
					None
				};
				let remove = if self.ref_counted {
					let removed = !tables.value[existing_tier].write_dec_ref(existing_address.offset(), log)?;
					log::trace!(target: "parity-db", "{}: Dereference {}, deleted={}", table.id, hex(key), removed);
					removed
				} else {
					log::trace!(target: "parity-db", "{}: Deleting {}", table.id, hex(key));
					tables.value[existing_tier].write_remove_plan(existing_address.offset(), log)?;
					true
				};
				if remove {
					if let Some((cur_size, compressed_size)) = cur_size {
						self.stats.remove_val(cur_size, compressed_size);
					}
					table.write_remove_plan(key, sub_index, log)?;
				}
				return Ok(PlanOutcome::Written);
			}
			log::trace!(target: "parity-db", "{}: Deletion missed {}", tables.index.id, hex(key));
			if self.collect_stats {
				self.stats.remove_miss();
			}
		}
		Ok(PlanOutcome::Skipped)
	}

	pub fn enact_plan(&self, action: LogAction, log: &mut LogReader) -> Result<()> {
		let tables = self.tables.read();
		let reindex = self.reindex.read();
		match action {
			LogAction::InsertIndex(record) => {
				if tables.index.id == record.table {
					tables.index.enact_plan(record.index, log)?;
				} else if let Some(table) = reindex.queue.iter().find(|r|r.id == record.table) {
					table.enact_plan(record.index, log)?;
				}
				else {
					log::warn!(
						target: "parity-db",
						"Missing table {}",
						record.table,
					);
					return Err(Error::Corruption("Missing table".into()));
				}
			},
			LogAction::InsertValue(record) => {
				tables.value[record.table.size_tier() as usize].enact_plan(record.index, log)?;
			}
			_ => panic!("Unexpected log action"),
		}
		Ok(())
	}

	pub fn validate_plan(&self, action: LogAction, log: &mut LogReader) -> Result<()> {
		let tables = self.tables.upgradable_read();
		let reindex = self.reindex.upgradable_read();
		match action {
			LogAction::InsertIndex(record) => {
				if tables.index.id == record.table {
					tables.index.validate_plan(record.index, log)?;
				} else if let Some(table) = reindex.queue.iter().find(|r|r.id == record.table) {
					table.validate_plan(record.index, log)?;
				}
				else {
					// Re-launch previously started reindex
					// TODO: add explicit log records for reindexing events.
					log::warn!(
						target: "parity-db",
						"Missing table {}, starting reindex",
						record.table,
					);
					Self::trigger_reindex(tables, reindex, self.path.as_path());
					return self.validate_plan(LogAction::InsertIndex(record), log);
				}
			},
			LogAction::InsertValue(record) => {
				tables.value[record.table.size_tier() as usize].validate_plan(record.index, log)?;
			}
			_ => panic!("Unexpected log action"),
		}
		Ok(())
	}

	pub fn complete_plan(&self, log: &mut LogWriter) -> Result<()> {
		let tables = self.tables.read();
		for t in tables.value.iter() {
			t.complete_plan(log)?;
		}
		if self.collect_stats {
			self.stats.commit()
		}
		Ok(())
	}

	pub fn refresh_metadata(&self) -> Result<()> {
		let tables = self.tables.read();
		for t in tables.value.iter() {
			t.refresh_metadata()?;
		}
		Ok(())
	}

	pub fn write_stats(&self, writer: &mut impl std::io::Write) {
		let tables = self.tables.read();
		tables.index.write_stats(&self.stats);
		self.stats.write_summary(writer, tables.index.id.col());
	}

	pub fn clear_stats(&self) {
		let tables = self.tables.read();
		let empty_stats = ColumnStats::empty();
		tables.index.write_stats(&empty_stats);
	}

	pub fn iter_while(&self, log: &Log, mut f: impl FnMut(IterState) -> bool) -> Result<()> {
		let action = |state | match state {
			IterStateOrCorrupted::Item(item) => Ok(f(item)),
			IterStateOrCorrupted::Corrupted( .. ) => Err(Error::Corruption("Missing indexed value".into())),
		};
		self.iter_while_inner(log, action, 0, true)
	}

	fn iter_while_inner(
		&self,
		log: &Log,
		mut f: impl FnMut(IterStateOrCorrupted) -> Result<bool>,
		start_chunk: u64,
		skip_preimage_indexes: bool,
	) -> Result<()> {
		let tables = self.tables.read();
		let source = &tables.index;

		if skip_preimage_indexes && self.preimage {
			// It is much faster to iterate over the value table than index.
			// We have to assume hashing scheme however.
			for table in &tables.value[..tables.value.len() - 1] {
				log::debug!( target: "parity-db", "{}: Iterating table {}", source.id, table.id);
				table.iter_while(&*log.overlays(), |index, rc, value, compressed| {
					let value = if compressed {
						self.decompress(&value)
					} else {
						value
					};
					let key = blake2_rfc::blake2b::blake2b(32, &[], &value);
					let key = self.hash(key.as_bytes());
					let state = IterStateOrCorrupted::Item(IterState { chunk_index: index, key, rc, value });
					f(state).unwrap_or(false)
				})?;
				log::debug!( target: "parity-db", "{}: Done Iterating table {}", source.id, table.id);
			}
		}

		for c in start_chunk .. source.id.total_chunks() {
			let entries = source.entries(c, &*log.overlays());
			for entry in entries.iter() {
				if entry.is_empty() {
					continue;
				}
				let (size_tier, offset) = if self.db_version >= 4 {
					let address = entry.address(source.id.index_bits());
					(address.size_tier(), address.offset())
				} else {
					let addr_bits = source.id.index_bits() + 10;
					let address = Address::from_u64(entry.as_u64() & ((1u64 << addr_bits) - 1));
					let size_tier = (address.as_u64() & 0x0f) as u8;
					let offset = address.as_u64() >> 4;
					(size_tier, offset)
				};

				if skip_preimage_indexes && self.preimage && size_tier as usize != tables.value.len() - 1 {
					continue;
				}
				let value = tables.value[size_tier as usize].get_with_meta(offset, &*log.overlays());
				let (value, rc, pk, compressed) = match value {
					Ok(Some(v)) => v,
					Ok(None) => {
						f(IterStateOrCorrupted::Corrupted(*entry, None))?;
						continue;
					},
					Err(e) => {
						f(IterStateOrCorrupted::Corrupted(*entry, Some(e)))?;
						continue;
					},
				};
				let mut key = source.recover_key_prefix(c, *entry);
				key[6..].copy_from_slice(&pk);
				let value = if compressed {
						self.decompress(&value)
				} else {
					value
				};
				log::debug!(
					target: "parity-db",
					"{}: Iterating at {}/{}, key={:?}, pk={:?}",
					source.id,
					c,
					source.id.total_chunks(),
					hex(&key),
					hex(&pk),
				);
				let state = IterStateOrCorrupted::Item(IterState { chunk_index: c, key, rc, value });
				if !f(state)? {
					return Ok(())
				}
			}
		}
		Ok(())
	}

	pub(crate) fn check_from_index(&self, log: &Log, check_param: &crate::CheckOptions, col: ColId) -> Result<()> {
		let start_chunk = check_param.from.unwrap_or(0);
		let end_chunk = check_param.bound;

		let step = 1000;
		let start_time = std::time::Instant::now();
		log::info!(target: "parity-db", "Starting full index iteration at {:?}", start_time);
		log::info!(target: "parity-db", "for {} chunks of column {}", self.tables.read().index.id.total_chunks(), col);
		self.iter_while_inner(log, |state| match state {
			IterStateOrCorrupted::Item(IterState { chunk_index, key, rc, value }) => {
				if Some(chunk_index) == end_chunk {
					return Ok(false);
				}
				if chunk_index % step == 0 {
					log::info!(target: "parity-db", "Chunk iteration at {}", chunk_index);
				}

				match check_param.display {
					CheckDisplay::Full => {
						log::info!("Index key: {:x?}\n \
							\tRc: {}",
							&key,
							rc,
						);
						log::info!("Value: {}", hex(&value));
					},
					CheckDisplay::Short(t) => {
						log::info!("Index key: {:x?}", &key);
						log::info!("Rc: {}, Value len: {}", rc, value.len());
						log::info!("Value: {}", hex(&value[..std::cmp::min(t as usize, value.len())]));
					},
					CheckDisplay::None => (),
				}
				Ok(true)
			},
			IterStateOrCorrupted::Corrupted(entry, e) => {
				log::info!("Corrupted value for index entry: {}:\n\t{:?}", entry.as_u64(), e);
				Ok(true)
			},
		}, start_chunk, false)?;

		log::info!(target: "parity-db", "Ended full index check, elapsed {:?}", start_time.elapsed());
		Ok(())
	}

	pub fn reindex(&self, log: &Log) -> Result<(Option<IndexTableId>, Vec<(Key, Address)>)> {
		// TODO: handle overlay
		let tables = self.tables.read();
		let reindex = self.reindex.read();
		let mut plan = Vec::new();
		let mut drop_index = None;
		if let Some(source) = reindex.queue.front() {
			let progress = reindex.progress.load(Ordering::Relaxed);
			if progress != source.id.total_chunks() {
				let mut source_index = progress;
				if source_index % 500 == 0 {
					log::debug!(target: "parity-db", "{}: Reindexing at {}/{}", tables.index.id, source_index, source.id.total_chunks());
				}
				log::debug!(target: "parity-db", "{}: Continue reindex at {}/{}", tables.index.id, source_index, source.id.total_chunks());
				while source_index < source.id.total_chunks() && plan.len() < MAX_REBALANCE_BATCH {
					log::trace!(target: "parity-db", "{}: Reindexing {}", source.id, source_index);
					let entries = source.entries(source_index, &*log.overlays());
					for entry in entries.iter() {
						if entry.is_empty() {
							continue;
						}
						// We only need key prefix to reindex.
						let key = source.recover_key_prefix(source_index, *entry);
						plan.push((key, entry.address(source.id.index_bits())))
					}
					source_index += 1;
				}
				log::trace!(target: "parity-db", "{}: End reindex batch {} ({})", tables.index.id, source_index, plan.len());
				reindex.progress.store(source_index, Ordering::Relaxed);
				if source_index == source.id.total_chunks() {
					log::info!(target: "parity-db", "Completed reindex {} into {}", source.id, tables.index.id);
					drop_index = Some(source.id);
				}
			}
		}
		Ok((drop_index, plan))
	}

	pub fn drop_index(&self, id: IndexTableId) -> Result<()> {
		log::debug!(target: "parity-db", "Dropping {}", id);
		let mut reindex = self.reindex.write();
		if reindex.queue.front_mut().map_or(false, |index| index.id == id) {
			let table = reindex.queue.pop_front();
			reindex.progress.store(0, Ordering::Relaxed);
			table.unwrap().drop_file()?;
		} else {
			log::warn!(target: "parity-db", "Dropping invalid index {}", id);
			return Ok(());
		}
		log::debug!(target: "parity-db", "Dropped {}", id);
		Ok(())
	}
}
