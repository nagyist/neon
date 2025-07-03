//! Hash table implementation on top of 'shmem'
//!
//! Features required in the long run by the communicator project:
//!
//! [X] Accessible from both Postgres processes and rust threads in the communicator process
//! [X] Low latency
//! [ ] Scalable to lots of concurrent accesses (currently relies on caller for locking)
//! [ ] Resizable

use std::fmt::Debug;
use std::hash::{Hash, Hasher, BuildHasher};
use std::mem::MaybeUninit;

use rustc_hash::FxBuildHasher;

use crate::shmem::ShmemHandle;

mod core;
pub mod entry;

#[cfg(test)]
mod tests;

mod optim;

use core::{CoreHashMap, INVALID_POS};
use entry::{Entry, OccupiedEntry};


pub struct HashMapInit<'a, K, V, S = rustc_hash::FxBuildHasher> {
    // Hash table can be allocated in a fixed memory area, or in a resizeable ShmemHandle.
    shmem_handle: Option<ShmemHandle>,
    shared_ptr: *mut HashMapShared<'a, K, V>,
	shared_size: usize,
	hasher: S,
	num_buckets: u32,
}

pub struct HashMapAccess<'a, K, V, S = rustc_hash::FxBuildHasher> {
    shmem_handle: Option<ShmemHandle>,
    shared_ptr: *mut HashMapShared<'a, K, V>,
	hasher: S,
}

unsafe impl<'a, K: Sync, V: Sync, S> Sync for HashMapAccess<'a, K, V, S> {}
unsafe impl<'a, K: Send, V: Send, S> Send for HashMapAccess<'a, K, V, S> {}

impl<'a, K: Clone + Hash + Eq, V, S> HashMapInit<'a, K, V, S> {
	pub fn with_hasher(self, hasher: S) -> HashMapInit<'a, K, V, S> {
		Self { hasher, ..self }
	}
	
	pub fn estimate_size(num_buckets: u32) -> usize {
        // add some margin to cover alignment etc.
        CoreHashMap::<K, V>::estimate_size(num_buckets) + size_of::<HashMapShared<K, V>>() + 1000
    }
	
    pub fn attach_writer(self) -> HashMapAccess<'a, K, V, S> {
		let mut ptr: *mut u8 = self.shared_ptr.cast();
        let end_ptr: *mut u8 = unsafe { ptr.add(self.shared_size) };
        ptr = unsafe { ptr.add(ptr.align_offset(align_of::<HashMapShared<K, V>>())) };
        let shared_ptr: *mut HashMapShared<K, V> = ptr.cast();
        ptr = unsafe { ptr.add(size_of::<HashMapShared<K, V>>()) };
 
        // carve out the buckets
        ptr = unsafe { ptr.byte_add(ptr.align_offset(align_of::<core::LinkedKey<K>>())) };
        let keys_ptr = ptr;
        ptr = unsafe { ptr.add(size_of::<core::LinkedKey<K>>() * self.num_buckets as usize) };
		
        ptr = unsafe { ptr.byte_add(ptr.align_offset(align_of::<Option<V>>())) };
        let vals_ptr = ptr;
        ptr = unsafe { ptr.add(size_of::<Option<V>>() * self.num_buckets as usize) };
		
        // use remaining space for the dictionary
        ptr = unsafe { ptr.byte_add(ptr.align_offset(align_of::<u32>())) };
        assert!(ptr.addr() < end_ptr.addr());
        let dictionary_ptr = ptr;
        let dictionary_size = unsafe { end_ptr.byte_offset_from(ptr) / size_of::<u32>() as isize };
        assert!(dictionary_size > 0);

        let keys =
            unsafe { std::slice::from_raw_parts_mut(keys_ptr.cast(), self.num_buckets as usize) };
		let vals =
            unsafe { std::slice::from_raw_parts_mut(vals_ptr.cast(), self.num_buckets as usize) };
        let dictionary = unsafe {
            std::slice::from_raw_parts_mut(dictionary_ptr.cast(), dictionary_size as usize)
        };
        let hashmap = CoreHashMap::new(keys, vals, dictionary);
        unsafe {
            std::ptr::write(shared_ptr, HashMapShared { inner: hashmap });
        }
		
        HashMapAccess {
            shmem_handle: self.shmem_handle,
            shared_ptr: self.shared_ptr,
			hasher: self.hasher,
        }
    }

    pub fn attach_reader(self) -> HashMapAccess<'a, K, V, S> {
        // no difference to attach_writer currently
         self.attach_writer()
    }
}

/// This is stored in the shared memory area
///
/// NOTE: We carve out the parts from a contiguous chunk. Growing and shrinking the hash table
/// relies on the memory layout! The data structures are laid out in the contiguous shared memory
/// area as follows:
///
/// HashMapShared
/// [buckets]
/// [dictionary]
///
/// In between the above parts, there can be padding bytes to align the parts correctly.
struct HashMapShared<'a, K, V> {
    inner: CoreHashMap<'a, K, V>	
}

impl<'a, K, V> HashMapInit<'a, K, V, rustc_hash::FxBuildHasher>
where
	K: Clone + Hash + Eq
{
	pub fn with_fixed(
		num_buckets: u32,
        area: &'a mut [MaybeUninit<u8>],
    ) -> HashMapInit<'a, K, V> {
		Self {
			num_buckets,
			shmem_handle: None,
			shared_ptr: area.as_mut_ptr().cast(),
			shared_size: area.len(),
			hasher: rustc_hash::FxBuildHasher::default(),
		}		
    }

    /// Initialize a new hash map in the given shared memory area
    pub fn with_shmem(num_buckets: u32, shmem: ShmemHandle) -> HashMapInit<'a, K, V> {
		let size = Self::estimate_size(num_buckets);
		shmem
            .set_size(size)
            .expect("could not resize shared memory area");
		Self {
			num_buckets,
			shared_ptr: shmem.data_ptr.as_ptr().cast(),
			shmem_handle: Some(shmem),
			shared_size: size,
			hasher: rustc_hash::FxBuildHasher::default()
		}
    }

	pub fn new_resizeable_named(num_buckets: u32, max_buckets: u32, name: &str) -> HashMapInit<'a, K, V> {
		let size = Self::estimate_size(num_buckets);
		let max_size = Self::estimate_size(max_buckets);
		let shmem = ShmemHandle::new(name, size, max_size)
			.expect("failed to make shared memory area");
		
		Self {
			num_buckets,
			shared_ptr: shmem.data_ptr.as_ptr().cast(),
			shmem_handle: Some(shmem),
			shared_size: size,
			hasher: rustc_hash::FxBuildHasher::default()
		}
	}

	pub fn new_resizeable(num_buckets: u32, max_buckets: u32) -> HashMapInit<'a, K, V> {
		use std::sync::atomic::{AtomicUsize, Ordering};
		const COUNTER: AtomicUsize = AtomicUsize::new(0);
		let val = COUNTER.fetch_add(1, Ordering::Relaxed);
		let name = format!("neon_shmem_hmap{}", val);
		Self::new_resizeable_named(num_buckets, max_buckets, &name)
	}
}

impl<'a, K, V, S: BuildHasher> HashMapAccess<'a, K, V, S>
where
    K: Clone + Hash + Eq,
{
    pub fn get_hash_value(&self, key: &K) -> u64 {
		self.hasher.hash_one(key)        
    }

    pub fn get_with_hash<'e>(&'e self, key: &K, hash: u64) -> Option<&'e V> {
        let map = unsafe { self.shared_ptr.as_ref() }.unwrap();

        map.inner.get_with_hash(key, hash)
    }

    pub fn entry_with_hash(&mut self, key: K, hash: u64) -> Entry<'a, '_, K, V> {
        let map = unsafe { self.shared_ptr.as_mut() }.unwrap();

        map.inner.entry_with_hash(key, hash)
    }

    pub fn remove_with_hash(&mut self, key: &K, hash: u64) {
        let map = unsafe { self.shared_ptr.as_mut() }.unwrap();

        match map.inner.entry_with_hash(key.clone(), hash) {
            Entry::Occupied(e) => {
                e.remove();
            }
            Entry::Vacant(_) => {}
        };
    }

    pub fn entry_at_bucket(&mut self, pos: usize) -> Option<OccupiedEntry<'a, '_, K, V>> {
        let map = unsafe { self.shared_ptr.as_mut() }.unwrap();
        map.inner.entry_at_bucket(pos)
    }

    pub fn get_num_buckets(&self) -> usize {
        let map = unsafe { self.shared_ptr.as_ref() }.unwrap();
        map.inner.get_num_buckets()
    }

    /// Return the key and value stored in bucket with given index. This can be used to
    /// iterate through the hash map. (An Iterator might be nicer. The communicator's
    /// clock algorithm needs to _slowly_ iterate through all buckets with its clock hand,
    /// without holding a lock. If we switch to an Iterator, it must not hold the lock.)
    pub fn get_at_bucket(&self, pos: usize) -> Option<(&K, &V)> {
        let map = unsafe { self.shared_ptr.as_ref() }.unwrap();

        if pos >= map.inner.keys.len() {
            return None;
        }
        let key = &map.inner.keys[pos];
		key.inner.as_ref().map(|k| (k, map.inner.vals[pos].as_ref().unwrap()))
    }

    pub fn get_bucket_for_value(&self, val_ptr: *const V) -> usize {
        let map = unsafe { self.shared_ptr.as_ref() }.unwrap();

        let origin = map.inner.vals.as_ptr();
        let idx = (val_ptr as usize - origin as usize) / (size_of::<V>() as usize);
        assert!(idx < map.inner.vals.len());

        idx
    }

    // for metrics
    pub fn get_num_buckets_in_use(&self) -> usize {
        let map = unsafe { self.shared_ptr.as_ref() }.unwrap();
        map.inner.buckets_in_use as usize
    }

	pub fn clear(&mut self) {
		let map = unsafe { self.shared_ptr.as_mut() }.unwrap();
        let inner = &mut map.inner;
        inner.clear()
	}
	
	/// Helper function that abstracts the common logic between growing and shrinking.
	/// The only significant difference in the rehashing step is how many buckets to rehash.
	fn rehash_dict(
		&mut self,
		inner: &mut CoreHashMap<'a, K, V>,
		keys_ptr: *mut core::LinkedKey<K>,
		end_ptr: *mut u8,
		num_buckets: u32,
		rehash_buckets: u32,
	) {
		inner.free_head = INVALID_POS;
		
		// Recalculate the dictionary
        let keys;
        let dictionary;
        unsafe {
            let keys_end_ptr = keys_ptr.add(num_buckets as usize);
            let buckets_end_ptr: *mut u8 = (keys_end_ptr as *mut u8)
				.add(size_of::<Option<V>>() * num_buckets as usize);
			let dictionary_ptr: *mut u32 = buckets_end_ptr
				.byte_add(buckets_end_ptr.align_offset(align_of::<u32>()))
                .cast();
            let dictionary_size: usize =
                end_ptr.byte_offset_from(buckets_end_ptr) as usize / size_of::<u32>();

            keys = std::slice::from_raw_parts_mut(keys_ptr, num_buckets as usize);
            dictionary = std::slice::from_raw_parts_mut(dictionary_ptr, dictionary_size);
        }
        for i in 0..dictionary.len() {
            dictionary[i] = INVALID_POS;
        }

        for i in 0..rehash_buckets as usize {
			if keys[i].inner.is_none() {
				keys[i].next = inner.free_head;
				inner.free_head = i as u32;
				continue;
			}

			let hash = self.hasher.hash_one(&keys[i].inner.as_ref().unwrap());
            let pos: usize = (hash % dictionary.len() as u64) as usize;
            keys[i].next = dictionary[pos];
            dictionary[pos] = i as u32;
        }

        // Finally, update the CoreHashMap struct
        inner.dictionary = dictionary;
        inner.keys = keys;
	}

	/// Rehash the map. Intended for benchmarking only.
	pub fn shuffle(&mut self) {
		let map = unsafe { self.shared_ptr.as_mut() }.unwrap();
        let inner = &mut map.inner;
		let num_buckets = inner.get_num_buckets() as u32;
		let size_bytes = HashMapInit::<K, V, S>::estimate_size(num_buckets);
		let end_ptr: *mut u8 = unsafe { (self.shared_ptr as *mut u8).add(size_bytes) };
        let keys_ptr = inner.keys.as_mut_ptr();
		self.rehash_dict(inner, keys_ptr, end_ptr, num_buckets, num_buckets);
	}

	
    // /// Grow
    // ///
    // /// 1. grow the underlying shared memory area
    // /// 2. Initialize new buckets. This overwrites the current dictionary
    // /// 3. Recalculate the dictionary
    // pub fn grow(&mut self, num_buckets: u32) -> Result<(), crate::shmem::Error> {
    //     let map = unsafe { self.shared_ptr.as_mut() }.unwrap();
    //     let inner = &mut map.inner;
    //     let old_num_buckets = inner.buckets.len() as u32;
    //     if num_buckets < old_num_buckets {
    //         panic!("grow called with a smaller number of buckets");
    //     }
    //     if num_buckets == old_num_buckets {
    //         return Ok(());
    //     }
    //     let shmem_handle = self
    //         .shmem_handle
    //         .as_ref()
    //         .expect("grow called on a fixed-size hash table");

    //     let size_bytes = HashMapInit::<K, V, S>::estimate_size(num_buckets);
    //     shmem_handle.set_size(size_bytes)?;
    //     let end_ptr: *mut u8 = unsafe { shmem_handle.data_ptr.as_ptr().add(size_bytes) };

    //     // Initialize new buckets. The new buckets are linked to the free list. NB: This overwrites
    //     // the dictionary!
    //     let keys_ptr = inner.keys.as_mut_ptr();
    //     unsafe {
    //         for i in old_num_buckets..num_buckets {
    //             let bucket_ptr = buckets_ptr.add(i as usize);
    //             bucket_ptr.write(core::Bucket {
    //                 next: if i < num_buckets-1 {
    //                     i as u32 + 1
    //                 } else {
    //                     inner.free_head
    //                 },
	// 				prev: if i > 0 {
	// 					PrevPos::Chained(i as u32 - 1)
	// 				} else {
	// 					PrevPos::First(INVALID_POS)
	// 				},
    //                 inner: None,
    //             });
    //         }
    //     }
	// 	self.rehash_dict(inner, keys_ptr, end_ptr, num_buckets, old_num_buckets);
    //     inner.free_head = old_num_buckets;

    //     Ok(())
    // }

	// /// Begin a shrink, limiting all new allocations to be in buckets with index less than `num_buckets`. 
// 	pub fn begin_shrink(&mut self, num_buckets: u32) {
// 		let map = unsafe { self.shared_ptr.as_mut() }.unwrap();
// 		if num_buckets > map.inner.get_num_buckets() as u32 {
//             panic!("shrink called with a larger number of buckets");
//         }
// 		_ = self
//             .shmem_handle
//             .as_ref()
//             .expect("shrink called on a fixed-size hash table");
// 		map.inner.alloc_limit = num_buckets;
// 	}

// 	/// Complete a shrink after caller has evicted entries, removing the unused buckets and rehashing.
// 	pub fn finish_shrink(&mut self) -> Result<(), crate::shmem::Error> {
// 		let map = unsafe { self.shared_ptr.as_mut() }.unwrap();
// 		let inner = &mut map.inner;
// 		if !inner.is_shrinking() {
// 			panic!("called finish_shrink when no shrink is in progress");
// 		}

// 		let num_buckets = inner.alloc_limit; 

// 		if inner.get_num_buckets() == num_buckets as usize {
//             return Ok(());
//         }
		
// 		for i in (num_buckets as usize)..inner.buckets.len() {
// 			if inner.buckets[i].inner.is_some() {
// 				// TODO(quantumish) Do we want to treat this as a violation of an invariant
// 				// or a legitimate error the caller can run into? Originally I thought this
// 				// could return something like a UnevictedError(index) as soon as it runs
// 				// into something (that way a caller could clear their soon-to-be-shrinked 
// 				// buckets by repeatedly trying to call `finish_shrink`). 
// 				//
// 				// Would require making a wider error type enum with this and shmem errors.
// 				panic!("unevicted entries in shrinked space")
// 			}
// 			match inner.buckets[i].prev {
// 				PrevPos::First(_) => {
// 					let next_pos = inner.buckets[i].next;
// 					inner.free_head = next_pos;
// 					if next_pos != INVALID_POS {
// 						inner.buckets[next_pos as usize].prev = PrevPos::First(INVALID_POS);
// 					}
// 				},
// 				PrevPos::Chained(j) => {
// 					let next_pos = inner.buckets[i].next;
// 					inner.buckets[j as usize].next = next_pos;
// 					if next_pos != INVALID_POS {
// 						inner.buckets[next_pos as usize].prev = PrevPos::Chained(j);
// 					}
// 				}
// 			}
// 		}

//         let shmem_handle = self
//             .shmem_handle
//             .as_ref()
//             .expect("shrink called on a fixed-size hash table");

// 		let size_bytes = HashMapInit::<K, V, S>::estimate_size(num_buckets);
//         shmem_handle.set_size(size_bytes)?;
//         let end_ptr: *mut u8 = unsafe { shmem_handle.data_ptr.as_ptr().add(size_bytes) };
// 		let buckets_ptr = inner.buckets.as_mut_ptr();
// 		self.rehash_dict(inner, buckets_ptr, end_ptr, num_buckets, num_buckets);
// 		inner.alloc_limit = INVALID_POS;
		
// 		Ok(())
// 	}

}
