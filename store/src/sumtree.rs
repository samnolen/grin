// Copyright 2017 The Grin Developers
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Implementation of the persistent Backend for the prunable MMR sum-tree.

use memmap;

use std::cmp;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write, BufReader, BufRead, ErrorKind};
use std::path::Path;
use std::io::Read;

use core::core::pmmr::{self, Summable, Backend, HashSum, VecBackend};
use core::ser;

const PMMR_DATA_FILE: &'static str = "pmmr_dat.bin";
const PMMR_RM_LOG_FILE: &'static str = "pmmr_rm_log.bin";
const PMMR_PRUNED_FILE: &'static str = "pmmr_pruned.bin";

/// Maximum number of nodes in the remove log before it gets flushed
pub const RM_LOG_MAX_NODES: usize = 10000;

/// Wrapper for a file that can be read at any position (random read) but for
/// which writes are append only. Reads are backed by a memory map (mmap(2)),
/// relying on the operating system for fast access and caching. The memory
/// map is reallocated to expand it when new writes are flushed.
struct AppendOnlyFile {
	path: String,
	file: File,
	mmap: Option<memmap::Mmap>,
}

impl AppendOnlyFile {
	/// Open a file (existing or not) as append-only, backed by a mmap.
	fn open(path: String) -> io::Result<AppendOnlyFile> {
		let file = OpenOptions::new()
			.read(true)
			.append(true)
			.create(true)
			.open(path.clone())?;
		Ok(AppendOnlyFile {
			path: path,
			file: file,
			mmap: None,
		})
	}

	/// Append data to the file.
	fn append(&mut self, buf: &[u8]) -> io::Result<()> {
		self.file.write_all(buf)
	}

	/// Syncs all writes (fsync), reallocating the memory map to make the newly
	/// written data accessible.
	fn sync(&mut self) -> io::Result<()> {
		self.file.sync_data()?;
		self.mmap = Some(unsafe {
			memmap::file(&self.file)
				.protection(memmap::Protection::Read)
				.map()?
		});
		Ok(())
	}

	/// Read length bytes of data at offset from the file. Leverages the memory
	/// map.
	fn read(&self, offset: usize, length: usize) -> Vec<u8> {
		if let None = self.mmap {
			return vec![];
		}
		let mmap = self.mmap.as_ref().unwrap();
		(&mmap[offset..(offset + length)]).to_vec()
	}

	/// Saves a copy of the current file content, skipping data at the provided
	/// prune indices. The prune Vec must be ordered.
	fn save_prune(&self, target: String, prune_offs: Vec<u64>, prune_len: u64) -> io::Result<()> {
		let mut reader = File::open(self.path.clone())?;
		let mut writer = File::create(target)?;

		// align the buffer on prune_len to avoid misalignments
		let mut buf = vec![0; (prune_len * 256) as usize];
		let mut read = 0;
		let mut prune_pos = 0;
		loop {
			// fill our buffer
			let len = match reader.read(&mut buf) {
				Ok(0) => return Ok(()),
				Ok(len) => len,
				Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
				Err(e) => return Err(e),
			} as u64;

			// write the buffer, except if we prune offsets in the current span,
			// in which case we skip 
			let mut buf_start = 0;
			while prune_offs[prune_pos] >= read && prune_offs[prune_pos] < read + len {
				let prune_at = prune_offs[prune_pos] as usize;
				if prune_at != buf_start {
					writer.write_all(&buf[buf_start..prune_at])?;
				}
				buf_start = prune_at + (prune_len as usize);
				if prune_offs.len() > prune_pos + 1 {
					prune_pos += 1;
				} else {
					break;
				}
			}
			writer.write_all(&mut buf[buf_start..(len as usize)])?;
			read += len;
		}
	}

	/// Current size of the file in bytes.
	fn size(&self) -> io::Result<u64> {
		fs::metadata(&self.path).map(|md| md.len())
	}
}

/// Log file fully cached in memory containing all positions that should be
/// eventually removed from the MMR append-only data file. Allows quick
/// checking of whether a piece of data has been marked for deletion. When the
/// log becomes too long, the MMR backend will actually remove chunks from the
/// MMR data file and truncate the remove log.
struct RemoveLog {
	path: String,
	file: File,
	// Ordered vector of MMR positions that should get eventually removed.
	removed: Vec<u64>,
}

impl RemoveLog {
	/// Open the remove log file. The content of the file will be read in memory
	/// for fast checking.
	fn open(path: String) -> io::Result<RemoveLog> {
		let removed = read_ordered_vec(path.clone())?;
		let file = OpenOptions::new().append(true).create(true).open(path.clone())?;
		Ok(RemoveLog {
			path: path,
			file: file,
			removed: removed,
		})
	}

	/// Truncate and empties the remove log.
	fn truncate(&mut self) -> io::Result<()> {
		self.removed = vec![];
		self.file = File::create(self.path.clone())?;
		Ok(())
	}

	/// Append a set of new positions to the remove log. Both adds those
	/// positions
	/// to the ordered in-memory set and to the file.
	fn append(&mut self, elmts: Vec<u64>) -> io::Result<()> {
		for elmt in elmts {
			match self.removed.binary_search(&elmt) {
				Ok(_) => continue,
				Err(idx) => {
					self.file.write_all(&ser::ser_vec(&elmt).unwrap()[..])?;
					self.removed.insert(idx, elmt);
				}
			}
		}
		self.file.sync_data()
	}

	/// Whether the remove log currently includes the provided position.
	fn includes(&self, elmt: u64) -> bool {
		self.removed.binary_search(&elmt).is_ok()
	}

	/// Number of positions stored in the remove log.
	fn len(&self) -> usize {
		self.removed.len()
	}
}

/// PMMR persistent backend implementation. Relies on multiple facilities to
/// handle writing, reading and pruning.
///
/// * A main storage file appends HashSum instances as they come. This
/// AppendOnlyFile is also backed by a mmap for reads.
/// * An in-memory backend buffers the latest batch of writes to ensure the
/// PMMR can always read recent values even if they haven't been flushed to
/// disk yet.
/// * A remove log tracks the positions that need to be pruned from the
/// main storage file.
pub struct PMMRBackend<T>
where
	T: Summable + Clone,
{
	data_dir: String,
	hashsum_file: AppendOnlyFile,
	remove_log: RemoveLog,
	pruned_nodes: pmmr::PruneList,
	// buffers addition of new elements until they're fully written to disk
	buffer: VecBackend<T>,
	buffer_index: usize,
}

impl<T> Backend<T> for PMMRBackend<T>
where
	T: Summable + Clone,
{
	/// Append the provided HashSums to the backend storage.
	#[allow(unused_variables)]
	fn append(&mut self, position: u64, data: Vec<HashSum<T>>) -> Result<(), String> {
		self.buffer.append(
			position - (self.buffer_index as u64),
			data.clone(),
		)?;
		for hs in data {
			if let Err(e) = self.hashsum_file.append(&ser::ser_vec(&hs).unwrap()[..]) {
				return Err(format!(
					"Could not write to log storage, disk full? {:?}",
					e
				));
			}
		}
		Ok(())
	}

	/// Get a HashSum by insertion position
	fn get(&self, position: u64) -> Option<HashSum<T>> {
		// First, check if it's in our temporary write buffer
		let pos_sz = position as usize;
		if pos_sz - 1 >= self.buffer_index && pos_sz - 1 < self.buffer_index + self.buffer.len() {
			return self.buffer.get((pos_sz - self.buffer_index) as u64);
		}

		// Second, check if this position has been pruned in the remove log
		if self.remove_log.includes(position) {
			return None;
		}

		// Third, check if it's in the pruned list or its offset
		let shift = self.pruned_nodes.get_shift(position);
		if let None = shift {
			return None
		}

		// The MMR starts at 1, our binary backend starts at 0
		let pos = position - 1;

		// Must be on disk, doing a read at the correct position
		let record_len = 32 + T::sum_len();
		let file_offset = ((pos - shift.unwrap()) as usize) * record_len;
		let data = self.hashsum_file.read(file_offset, record_len);
		match ser::deserialize(&mut &data[..]) {
			Ok(hashsum) => Some(hashsum),
			Err(e) => {
				error!(
					"Corrupted storage, could not read an entry from sum tree store: {:?}",
					e
				);
				None
			}
		}
	}

	/// Remove HashSums by insertion position
	fn remove(&mut self, positions: Vec<u64>) -> Result<(), String> {
		if self.buffer.used_size() > 0 {
			self.buffer.remove(positions.clone()).unwrap();
		}
		self.remove_log.append(positions).map_err(|e| {
			format!("Could not write to log storage, disk full? {:?}", e)
		})
	}
}

impl<T> PMMRBackend<T>
where
	T: Summable + Clone,
{
	/// Instantiates a new PMMR backend that will use the provided directly to
	/// store its files.
	pub fn new(data_dir: String) -> io::Result<PMMRBackend<T>> {
		let hs_file = AppendOnlyFile::open(format!("{}/{}", data_dir, PMMR_DATA_FILE))?;
		let sz = hs_file.size()?;
		let record_len = 32 + T::sum_len();
		let rm_log = RemoveLog::open(format!("{}/{}", data_dir, PMMR_RM_LOG_FILE))?;
		let prune_list = read_ordered_vec(format!("{}/{}", data_dir, PMMR_PRUNED_FILE))?;

		Ok(PMMRBackend {
			data_dir: data_dir,
			hashsum_file: hs_file,
			remove_log: rm_log,
			buffer: VecBackend::new(),
			buffer_index: (sz as usize) / record_len,
			pruned_nodes: pmmr::PruneList{pruned_nodes: prune_list},
		})
	}

	/// Syncs all files to disk. A call to sync is required to ensure all the
	/// data has been successfully written to disk.
	pub fn sync(&mut self) -> io::Result<()> {
		self.buffer_index = self.buffer_index + self.buffer.len();
		self.buffer.clear();

		self.hashsum_file.sync()
	}

	/// Checks the length of the remove log to see if it should get compacted.
	/// If so, the remove log is flushed into the pruned list, which itself gets
	/// saved, and the main hashsum data file is rewritten, cutting the removed
	/// data.
	///
	/// If a max_len strictly greater than 0 is provided, the value will be used
	/// to decide whether the remove log has reached its maximum length,
	/// otherwise the RM_LOG_MAX_NODES default value is used.
	pub fn check_compact(&mut self, max_len: usize) -> io::Result<()> {
		if !(max_len > 0 && self.remove_log.len() > max_len ||
			max_len == 0 && self.remove_log.len() > RM_LOG_MAX_NODES) {
			return Ok(())
		}

		// 0. validate none of the nodes in the rm log are in the prune list (to
		// avoid accidental double compaction)
		for pos in &self.remove_log.removed[..] {
			if let None = self.pruned_nodes.pruned_pos(*pos) {
				// TODO we likely can recover from this by directly jumping to 3
				error!("The remove log contains nodes that are already in the pruned \
							 list, a previous compaction likely failed.");
				return Ok(());
			}
		}

		// 1. save hashsum file to a compact copy, skipping data that's in the
		// remove list
		let tmp_prune_file = format!("{}/{}.prune", self.data_dir, PMMR_DATA_FILE);
		let record_len = (32 + T::sum_len()) as u64;
		let to_rm = self.remove_log.removed.iter().map(|pos| {
			let shift = self.pruned_nodes.get_shift(*pos);
			(*pos - 1 - shift.unwrap()) * record_len
		}).collect();
		self.hashsum_file.save_prune(tmp_prune_file.clone(), to_rm, record_len)?;

		// 2. update the prune list and save it in place
		for rm_pos in &self.remove_log.removed[..] {
			self.pruned_nodes.add(*rm_pos);
		}
		write_vec(format!("{}/{}", self.data_dir, PMMR_PRUNED_FILE), &self.pruned_nodes.pruned_nodes)?;

		// 3. move the compact copy to the hashsum file and re-open it
		fs::rename(tmp_prune_file.clone(), format!("{}/{}", self.data_dir, PMMR_DATA_FILE))?;
		self.hashsum_file = AppendOnlyFile::open(format!("{}/{}", self.data_dir, PMMR_DATA_FILE))?;
		self.hashsum_file.sync()?;

		// 4. truncate the rm log
		//self.remove_log.truncate()?;

		Ok(())
	}
}

// Read an ordered vector of scalars from a file.
fn read_ordered_vec<T>(path: String) -> io::Result<Vec<T>>
	where T: ser::Readable + cmp::Ord {

	let file_path = Path::new(&path);
	let mut ovec = Vec::with_capacity(1000);
	if file_path.exists() {
		let mut file = BufReader::with_capacity(8 * 1000, File::open(path.clone())?);
		loop {
			// need a block to end mutable borrow before consume
			let buf_len = {
				let buf = file.fill_buf()?;
				if buf.len() == 0 {
					break;
				}
				let elmts_res: Result<Vec<T>, ser::Error> = ser::deserialize(&mut &buf[..]);
				match elmts_res {
					Ok(elmts) => {
						for elmt in elmts {
							if let Err(idx) = ovec.binary_search(&elmt) {
								ovec.insert(idx, elmt);
							}
						}
					}
					Err(_) => {
						return Err(io::Error::new(
							io::ErrorKind::InvalidData,
							format!("Corrupted storage, could not read file at {}", path),
						));
					}
				}
				buf.len()
			};
			file.consume(buf_len);
		}
	}
	Ok(ovec)
}

fn write_vec<T>(path: String, v: &Vec<T>) -> io::Result<()>
	where T: ser::Writeable {
	
	let mut file_path = File::create(&path)?;
	ser::serialize(&mut file_path, v).map_err(|_| {
		io::Error::new(
			io::ErrorKind::InvalidInput,
			format!("Failed to serialize data when writing to {}", path))
	})?;
	Ok(())
}