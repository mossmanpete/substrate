// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! This module implements a buddy allocation heap.
//! It uses a binary tree and follows the concepts outlined in
//! https://en.wikipedia.org/wiki/Buddy_memory_allocation.

extern crate fnv;

use std::vec;
use self::fnv::FnvHashMap;

// The pointers need to be aligned. By choosing a block size
// which is a multiple of the memory alignment requirement
// it is ensured that a pointer is always aligned. This is
// because in buddy allocation a pointer always points to the
// start of a block.
//
// In our case the alignment for wasm32-unknown-unknown is
// 1 byte though, i.e. the pointer will always be aligned.
const BLOCK_SIZE: u32 = 8192; // 2^13 Bytes

#[derive(PartialEq, Copy, Clone)]
enum Node {
	Free,
	Full,
	Split,
}

/// A buddy allocation heap, which tracks allocations and deallocations
/// using a binary tree.
pub struct Heap {
	allocated_bytes: FnvHashMap<u32, u32>,
	levels: u32,
	tree: vec::Vec<Node>,
	total_size: u32,
}

impl Heap {

	/// Creates a new buddy allocation heap with a fixed size (in Bytes).
	pub fn new(reserved: u32) -> Self {
		let leaves = reserved / BLOCK_SIZE;
		let levels = Heap::get_tree_levels(leaves);
		let node_count: usize = (1 << levels + 1) - 1;

		Heap {
			allocated_bytes: FnvHashMap::default(),
			levels,
			tree: vec![Node::Free; node_count],
			total_size: 0,
		}
	}

	/// Gets requested number of bytes to allocate and returns an index offset.
	/// The index offset starts at 0.
	pub fn allocate(&mut self, size: u32) -> u32 {
		// Get the requested level from number of blocks requested
		let blocks_needed = (size + BLOCK_SIZE - 1) / BLOCK_SIZE;
		let block_offset = match self.allocate_block_in_tree(blocks_needed) {
			Some(v) => v,
			None => return 0,
		};

		let ptr = BLOCK_SIZE * block_offset as u32;
		self.allocated_bytes.insert(ptr, size as u32);

		self.total_size += size;
		trace!(target: "wasm-heap", "Heap size over {} Bytes after allocation", self.total_size);

		ptr + 1
	}

	fn allocate_block_in_tree(&mut self, blocks_needed: u32) -> Option<usize> {
		let levels_needed = Heap::get_tree_levels(blocks_needed);
		if levels_needed > self.levels {
			trace!(target: "wasm-heap", "Heap is too small: {:?} > {:?}", levels_needed, self.levels);
			return None;
		}

		// Start at tree root and traverse down
		let mut index = 0;
		let mut current_level = self.levels;
		'down: loop {
			let buddy_exists = index & 1 == 1;

			if current_level == levels_needed {
				if self.tree[index] == Node::Free {
					self.tree[index] = Node::Full;

					if index > 0 {
						let parent = self.get_parent_node_index(index);
						self.update_parent_nodes(parent);
					}

					break 'down;
				}
			} else {
				match self.tree[index] {
					Node::Full => {
						if buddy_exists {
							// Check if buddy is free
							index += 1;
						} else {
							break 'down;
						}
						continue 'down;
					},

					Node::Free => {
						// If node is free we split it and descend further down
						self.tree[index] = Node::Split;
						index = index * 2 + 1;
						current_level -= 1;
						continue 'down;
					},

					Node::Split => {
						// Descend further
						index = index * 2 + 1;
						current_level -= 1;
						continue 'down;
					},
				}
			}

			if buddy_exists {
				// If a buddy exists it needs to be checked as well
				index += 1;
				continue 'down;
			}

			// Backtrack once we're at the bottom and haven't matched a free block yet
			'up: loop {
				if index == 0 {
					trace!(target: "wasm-heap", "Heap is too small: tree root reached.");
					return None;
				}

				index = self.get_parent_node_index(index);
				current_level += 1;
				let has_buddy = index & 1 == 1;
				if has_buddy {
					index += 1;
					break 'up;
				}
			}
		}

		let current_level_offset = (1 << self.levels - current_level) - 1;
		let level_offset = index - current_level_offset;

		let block_offset = level_offset * (1 << current_level);
		Some(block_offset as usize)
	}

	/// Deallocates all blocks which were allocated for a pointer.
	pub fn deallocate(&mut self, mut ptr: u32) {
		ptr -= 1;

		let allocated_size = match self.allocated_bytes.get(&ptr) {
			Some(v) => *v,

			// If nothing has been allocated for the pointer nothing happens
			None => return (),
		};

		let count_blocks = (allocated_size + BLOCK_SIZE - 1) / BLOCK_SIZE;
		let block_offset = ptr / BLOCK_SIZE;
		self.free(block_offset, count_blocks);
		self.allocated_bytes.remove(&ptr).unwrap_or_default();

		self.total_size = self.total_size.checked_sub(allocated_size).unwrap_or(0);
		trace!(target: "wasm-heap", "Heap size over {} Bytes after deallocation", self.total_size);
	}

	fn free(&mut self, block_offset: u32, count_blocks: u32) {
		let requested_level = Heap::get_tree_levels(count_blocks);
		let current_level_offset = (1 << self.levels - requested_level) - 1;
		let level_offset = block_offset / (1 << requested_level);
		let index_offset = current_level_offset + level_offset;

		if index_offset > self.tree.len() as u32 - 1 {
			trace!(target: "wasm-heap", "Index offset {} is > length of tree {}", index_offset, self.tree.len());
		}

		self.free_and_merge(index_offset as usize);

		let parent = self.get_parent_node_index(index_offset as usize);
		self.update_parent_nodes(parent);
	}

	fn get_parent_node_index(&mut self, index: usize) -> usize {
		(index + 1) / 2 - 1
	}

	fn free_and_merge(&mut self, index: usize) {
		self.tree[index] = Node::Free;

		if index == 0 {
			return;
		}

		let has_right_buddy = (index & 1) == 1;
		let other_node = if has_right_buddy {
			index + 1
		} else {
			index - 1
		};

		if self.tree[other_node] == Node::Free {
			let parent = self.get_parent_node_index(index);
			self.free_and_merge(parent);
		}
	}

	fn update_parent_nodes(&mut self, index: usize) {
		let left_child = index * 2 + 1;
		let right_child = index * 2 + 2;

		let children_free = self.tree[left_child] == Node::Free && self.tree[right_child] == Node::Free;
		let children_full = self.tree[left_child] == Node::Full && self.tree[right_child] == Node::Full;
		if children_free {
			self.tree[index] = Node::Free;
		} else if children_full {
			self.tree[index] = Node::Full;
		} else {
			self.tree[index] = Node::Split;
		}

		if index == 0 {
			// Tree root
			return;
		}

		let parent = self.get_parent_node_index(index);
		self.update_parent_nodes(parent);
	}

	fn get_tree_levels(mut count_blocks: u32) -> u32 {
		if count_blocks == 0 {
				0
		} else {
				let mut counter = 0;
				while {count_blocks >>= 1; count_blocks > 0} {
						counter += 1;
				}
				counter
		}
	}

}

#[cfg(test)]
mod tests {
	use heap::BLOCK_SIZE;

	#[test]
	fn first_pointer_should_be_one() {
		let mut heap = super::Heap::new(20);
		let ptr = heap.allocate(5);
		assert_eq!(ptr, 1);
	}

	#[test]
	fn deallocation_for_nonexistent_pointer_should_not_panic() {
		let mut heap = super::Heap::new(20);
		let ret = heap.deallocate(5);
		assert_eq!(ret, ());
	}

	#[test]
	fn should_calculate_tree_size_from_heap_size() {
		let heap_size = BLOCK_SIZE * 4;
		let heap = super::Heap::new(heap_size);

		assert_eq!(heap.levels, 2);
	}

	#[test]
	fn should_round_tree_size_to_nearest_possible() {
		let heap_size = BLOCK_SIZE * 4 + 1;
		let heap = super::Heap::new(heap_size);

		assert_eq!(heap.levels, 2);
	}

	#[test]
	fn heap_size_should_stay_zero_in_total() {
		let heap_size = BLOCK_SIZE * 4;
		let mut heap = super::Heap::new(heap_size);
		assert_eq!(heap.total_size, 0);

		let ptr = heap.allocate(42);
		assert_eq!(heap.total_size, 42);

		heap.deallocate(ptr);
		assert_eq!(heap.total_size, 0);
	}

	#[test]
	fn heap_size_should_stay_constant() {
		let heap_size = BLOCK_SIZE * 4;
		let mut heap = super::Heap::new(heap_size);
		for _ in 1..10 {
			assert_eq!(heap.total_size, 0);

			let ptr = heap.allocate(42);
			assert_eq!(ptr, 1);
			assert_eq!(heap.total_size, 42);

			heap.deallocate(ptr);
			assert_eq!(heap.total_size, 0);
		}

		assert_eq!(heap.total_size, 0);
	}

}
