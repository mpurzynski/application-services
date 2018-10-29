/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */


use std::sync::atomic::{
    Ordering,
    AtomicUsize,
};
use std::sync::{RwLock, Mutex};
use std::ops;
use into_ffi::IntoFfi;

/// [`HandleMap`] is a collection type which can hold any type of value, and offers a
/// stable handle which can be used to retrieve it on insertion. These handles
/// offer methods for converting [to](Handle::into_u64) and
/// [from](Handle::from_u64) 64 bit integers, meaning they're very easy to pass
/// over the FFI (they also implement [`IntoFfi`] for the same purpose).
///
/// ## Example
///
/// In FFI code, we expect the typical usage pattern will be:
///
/// ```rust,no_run
/// # #[macro_use] extern crate lazy_static;
/// # extern crate ffi_support;
/// # use ffi_support::*;
/// # use std::sync::*;
///
/// // Somewhere...
/// struct Thing { value: f64 }
///
/// lazy_static! {
///     static ref ITEMS: RwLock<HandleMap<Mutex<Thing>>> = RwLock::new(HandleMap::new());
/// }
///
/// #[no_mangle]
/// pub extern "C" fn mylib_new_thing(value: f64, err: &mut ExternError) -> u64 {
///     // most of the time you'll actually want `call_with_result`, but we can't
///     // fail to construct a `Thing`.
///     call_with_output(err, || {
///         let mut map = ITEMS.write().unwrap();
///         map.insert(Mutex::new(Thing { value }))
///     })
/// }
///
/// #[no_mangle]
/// pub extern "C" fn mylib_thing_value(h: u64, err: &mut ExternError) -> f64 {
///     call_with_output(err, || {
///         let mut map = ITEMS.read().expect("Poisoned map");
///         let handle = Handle::from_u64(h).expect("Invalid handle");
///         let mtx = map.get(handle).expect("Invalid handle");
///         let val = mtx.lock().expect("Poisoned map");
///         val.value
///     })
/// }
///
/// #[no_mangle]
/// pub extern "C" fn mylib_thing_set_value(h: u64, new_value: f64, err: &mut ExternError) {
///     call_with_output(err, || {
///         let mut map = ITEMS.read().expect("Poisoned map");
///         let handle = Handle::from_u64(h).expect("Invalid handle");
///         let mtx = map.get(handle).expect("Invalid handle");
///         let mut val = mtx.lock().expect("Poisoned map");
///         val.value = new_value;
///     })
/// }
///
/// #[no_mangle]
/// pub extern "C" fn mylib_destroy_thing(h: u64, err: &mut ExternError) {
///     call_with_output(err, || {
///         let mut map = ITEMS.write().expect("Poisoned map");
///         let handle = Handle::from_u64(h).expect("Invalid handle");
///         map.delete(handle).expect("Value already deleted (or handle is bad)");
///     })
/// }
/// ```
///
/// ## Comparison to types from other crates
///
/// `HandleMap` is similar to types offered by other crates.
///
/// This type is similar to types offered by crates such as `slotmap`, or `slab`
/// with the following additional benefits:
///
/// 1. Unlike `slab` (but like slotmap), we implement versioning, detecting ABA
///    problems, which allows us to detect use after free.
/// 2. Unlike `slotmap`, we don't have the `T: Copy` restriction.
/// 3. Unlike either, we can detect when you use a Key in a map that did not
///    allocate the key. This is true even when the map is from a `.so` file
///    compiled separately.
/// 4. Our implementation is likely slower, but doesn't use any `unsafe` (at the
///    time of this writing, at least).
///
/// And following drawbacks:
///
/// 1. `slotmap` holds its version information in a `u32`, and so it takes
///    2<sup>31</sup> colliding insertions and deletions before it could
///    potentially fail to detect an ABA issue, wheras we use a `u16`, and are
///    limited to 2<sup>15</sup>.
/// 2. Similarly, we can only hold 2<sup>16</sup> items at once, unlike
///    `slotmap`'s 2<sup>32</sup>. (Considering these items are typically things
///    like database handles, this is probably plenty).
///
/// Both of these issues seem exceptionally unlikely, even for extremely
/// long-lived `HandleMap`, and we're still memory safe even if they occur (we
/// just might fail to notice a bug).
#[derive(Debug, Clone)]
pub struct HandleMap<T> {
    // The value of `map_id` in each `Handle`.
    id: u16,

    // Index to the start of the free list. Always points to a free item --
    // we never allow our free list to become empty.
    first_free: u16,

    // The number of entries with `data.is_some()`. This is never equal to
    // `entries.len()`, we always grow before that point to ensure we always have
    // a valid `first_free` index to add entries onto. This is our `len`.
    num_entries: usize,

    // The actual data. Note: entries.len() is our 'capacity'.
    entries: Vec<Entry<T>>,
}

// Entry's version/index fields are u16 becuase ultimately we're returning this
// over the FFI as a 64 bit int. Using usize would perhaps be more idiomatic
// for indices (and arbitrary counters like version), but using the actual type
// we're constrained to makes it harder to forget.
#[derive(Debug, Clone)]
struct Entry<T> {
    // Note: always even for occupied values.
    version: u16,
    state: EntryState<T>,
}

#[derive(Debug, Clone)]
enum EntryState<T> {
    // Not part of the free list
    Active(T),
    // The u16 is the next index in the free list.
    InFreeList(u16),
    // Part of the free list, but the sentinel.
    EndOfFreeList,
}

impl<T> EntryState<T> {
    #[inline]
    fn is_end_of_list(&self) -> bool {
        match self {
            EntryState::EndOfFreeList => true,
            _ => false
        }
    }

    #[inline]
    fn is_occupied(&self) -> bool {
        self.get_item().is_some()
    }

    #[inline]
    fn get_item(&self) -> Option<&T> {
        match self {
            EntryState::Active(v) => Some(v),
            _ => None
        }
    }

    #[inline]
    fn get_item_mut(&mut self) -> Option<&mut T> {
        match self {
            EntryState::Active(v) => Some(v),
            _ => None
        }
    }
}

// Small helper to check our casts.
#[inline]
fn to_u16(v: usize) -> u16 {
    use std::u16::{MAX as U16_MAX};
    // Shouldn't ever happen.
    assert!(v <= (U16_MAX as usize), "Bug: Doesn't fit in u16: {}", v);
    v as u16
}

/// The maximum capacity of a [`HandleMap`]. Attempting to instantiate one with
/// a larger capacity will cause a panic.
///
/// Note: This could go as high as `(1 << 16) - 2`, but doing is seems more
/// error prone. For the sake of paranoia, we limit it to this size, which is
/// already quite a bit larger than it seems like we're likely to ever need.
pub const MAX_CAPACITY: usize = (1 << 15) - 1;

// Never having to worry about capacity == 0 simplifies the code at the cost of
// worse memory usage. It doesn't seem like there's any reason to make this
// public.
const MIN_CAPACITY: usize = 4;

/// An error representing the ways a `Handle` may be invalid.
// TODO: Should we implement Into<ExternError> for this? Would require that
// we reserve an error code for it...
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Fail)]
pub enum HandleError {
    /// Returned from [`Handle::from_u64`] if [`Handle::is_valid`] fails.
    #[fail(display = "u64 could not encode a valid Handle")]
    InvalidHandle,

    /// Returned from get/get_mut/delete if the handle is stale (this indicates
    /// something equivalent to a use-after-free / double-free, etc).
    #[fail(display = "Handle has stale version number")]
    StaleVersion,

    /// Returned if the handle index references an index past the end of the
    /// HandleMap.
    #[fail(display = "Handle references a index past the end of this HandleMap")]
    IndexPastEnd,

    /// The handle has a map_id for a different map than the one it was
    /// attempted to be used with.
    #[fail(display = "Handle is from a different map")]
    WrongMap,
}

impl<T> HandleMap<T> {
    /// Create a new `HandleMap` with the default capacity.
    pub fn new() -> Self {
        Self::new_with_capacity(MIN_CAPACITY)
    }

    /// Allocate a new `HandleMap`. Note that the actual capacity may be larger
    /// than the requested value.
    ///
    /// Panics if `request` is greater than [`handle_map::MAX_CAPACITY`]
    pub fn new_with_capacity(request: usize) -> Self {
        assert!(request <= MAX_CAPACITY,
                "HandleMap capacity is limited to {} (request was {})",
                MAX_CAPACITY,
                request);

        let capacity = request.max(MIN_CAPACITY);
        let id = next_handle_map_id();
        let mut entries = Vec::with_capacity(capacity);

        // Initialize each entry with version 1, and as a member of the free list
        for i in 0..(capacity - 1) {
            entries.push(Entry {
                version: 1,
                state: EntryState::InFreeList(to_u16(i + 1)),
            });
        }

        // And the final entry is at the end of the free list
        // (but still has version 1).
        entries.push(Entry {
            version: 1,
            state: EntryState::EndOfFreeList
        });
        Self {
            id,
            first_free: 0,
            num_entries: 0,
            entries,
        }
    }

    /// Get the number of entries in the `HandleMap`.
    #[inline]
    pub fn len(&self) -> usize {
        self.num_entries
    }

    /// Returns the number of slots allocated in the handle map.
    #[inline]
    pub fn capacity(&self) -> usize {
        // It's not a bug that this isn't entries.capacity() -- We're returning
        // how many slots exist, not something about the backing memory allocation
        self.entries.len()
    }

    fn ensure_capacity(&mut self, cap_at_least: usize) {
        assert_ne!(self.len(), self.capacity(), "Bug: should have grown by now");
        assert!(cap_at_least <= MAX_CAPACITY, "HandleMap overfilled");
        if self.capacity() > cap_at_least {
            return;
        }

        let mut next_cap = self.capacity();
        while next_cap <= cap_at_least {
            next_cap *= 2;
        }
        next_cap = next_cap.min(MAX_CAPACITY);

        let need_extra = if next_cap > self.entries.capacity() {
            next_cap - self.entries.capacity()
        } else {
            0
        };

        self.entries.reserve(need_extra);

        assert!(!self.entries[self.first_free as usize].state.is_occupied(),
                "Bug: HandleMap.first_free points at occupied index");

        // Insert new entries at the front of our list.
        while self.entries.len() < next_cap - 1 {
            // This is a little wasteful but whatever. Add each new entry to the
            // front of the free list one at a time.
            self.entries.push(Entry {
                version: 1,
                state: EntryState::InFreeList(self.first_free)
            });
            self.first_free = to_u16(self.entries.len() - 1);
        }

        self.debug_check_valid();
    }

    #[inline]
    fn debug_check_valid(&self) {
        // Run the expensive validity check in tests and in debug builds.
        #[cfg(any(debug_assertions, test))] {
            self.assert_valid();
        }
    }

    #[cfg(any(debug_assertions, test))]
    fn assert_valid(&self) {
        assert_ne!(self.len(), self.capacity());
        assert!(self.capacity() <= MAX_CAPACITY, "Entries too large");
        // Validate that our free list is correct.

        let number_of_ends = self.entries.iter().filter(|e| e.state.is_end_of_list()).count();
        assert_eq!(number_of_ends, 1,
                   "More than one entry think's it's the end of the list, or no entries do");

        // Check that the free list hits every unoccupied item.
        // The tuple is: `(should_be_in_free_list, is_in_free_list)`.
        let mut free_indices = vec![(false, false); self.capacity()];
        for (i, e) in self.entries.iter().enumerate() {
            if !e.state.is_occupied() {
                free_indices[i].0 = true;
            }
        }

        let mut next = self.first_free;
        loop {
            let ni = next as usize;

            assert!(ni <= free_indices.len(),
                    "Free list contains out of bounds index!");

            assert!(free_indices[ni].0,
                    "Free list has an index that shouldn't be free! {}", ni);

            assert!(!free_indices[ni].1,
                    "Free list hit an index ({}) more than once! Cycle detected!", ni);

            free_indices[ni].1 = true;

            match &self.entries[ni].state {
                &EntryState::InFreeList(ref next_index) => next = *next_index,
                &EntryState::EndOfFreeList => break,
                // Hitting `Active` here is probably not possible because of the checks above, but who knows.
                &EntryState::Active(..) => panic!("Bug: Active item in free list at {}", next),
            }
        }
        let mut occupied_count = 0;
        for (i, &(should_be_free, is_free)) in free_indices.iter().enumerate() {
            assert_eq!(should_be_free, is_free,
                       "Free list missed item, or contains an item it shouldn't: {}", i);
            if !should_be_free {
                occupied_count += 1;
            }
        }
        assert_eq!(self.num_entries, occupied_count,
            "num_entries doesn't reflect the actual number of entries");
    }

    /// Insert an item into the map, and return a handle to it.
    pub fn insert(&mut self, v: T) -> Handle {
        let need_cap = self.len() + 1;
        self.ensure_capacity(need_cap);
        let index = self.first_free;
        let result = {
            // Scoped mutable borrow of entry.
            let entry = &mut self.entries[index as usize];
            let new_first_free = match entry.state {
                EntryState::InFreeList(i) => i,
                _ => panic!("Bug: next_index pointed at non-free list entry (or end of list)"),
            };
            entry.version += 1;
            if entry.version == 0 {
                entry.version += 2;
            }
            entry.state = EntryState::Active(v);
            self.first_free = new_first_free;
            self.num_entries += 1;

            Handle {
                map_id: self.id,
                version: entry.version,
                index,
            }
        };
        self.debug_check_valid();
        result
    }

    // Helper to contain the handle validation boilerplate. Returns `h.index as usize`.
    fn check_handle(&self, h: Handle) -> Result<usize, HandleError> {
        if h.map_id != self.id {
            info!("HandleMap access with handle having wrong map id: {:?} (our map id is {})",
                  h, self.id);
            return Err(HandleError::WrongMap);
        }
        let index = h.index as usize;
        if index >= self.entries.len() {
            info!("HandleMap accessed with handle past end of map: {:?}", h);
            return Err(HandleError::IndexPastEnd);
        }
        if self.entries[index].version != h.version {
            info!("HandleMap accessed with handle with wrong version {:?} (entry version is {})",
                  h, self.entries[index].version);
            return Err(HandleError::StaleVersion);
        }
        Ok(index)
    }

    /// Delete an item from the HandleMap.
    pub fn delete(&mut self, h: Handle) -> Result<(), HandleError> {
        let index = self.check_handle(h)?;
        {
            // Scoped mutable bororw of entry.
            let entry = &mut self.entries[index];
            assert!(entry.state.is_occupied(), "Bug: handle references unoccupied entry");

            entry.version += 1;
            let index = h.index;
            entry.state = EntryState::InFreeList(self.first_free);
            self.num_entries -= 1;
            self.first_free = index;
        }
        self.debug_check_valid();
        Ok(())
    }

    /// Get a reference to the item referenced by the handle, or return a
    /// [`HandleError`] describing the problem.
    pub fn get(&self, h: Handle) -> Result<&T, HandleError> {
        let idx = self.check_handle(h)?;
        let entry = &self.entries[idx];
        let item = entry.state.get_item().expect("Bug: Handle created with invalid version");
        Ok(item)
    }

    /// Get a mut reference to the item referenced by the handle, or return a
    /// [`HandleError`] describing the problem.
    pub fn get_mut(&mut self, h: Handle) -> Result<&mut T, HandleError> {
        let idx = self.check_handle(h)?;
        let entry = &mut self.entries[idx];
        let item = entry.state.get_item_mut().expect("Bug: Handle created with invalid version");
        Ok(item)
    }
}

impl<T> Default for HandleMap<T> {
    #[inline]
    fn default() -> Self {
        HandleMap::new()
    }
}

impl<T> ops::Index<Handle> for HandleMap<T> {
    type Output = T;
    #[inline]
    fn index(&self, h: Handle) -> &T {
        self.get(h).expect("Indexed into HandleMap with invalid handle!")
    }
}

// We don't implement IndexMut intentionally (implementing ops::Index is
// dubious enough)

/// A Handle we allow to be returned over the FFI by implementing [`IntoFfi`].
/// This type is intentionally not `#[repr(C)]`, and getting the data out of the
/// FFI is done using `Handle::from_u64`, or it's implemetation of `From<u64>`.
///
/// It consists of, at a minimum:
///
/// - A "map id" (used to ensure you're using it with the correct map)
/// - a "version" (incremented when the value in the index changes, used to
///   detect multiple frees, use after free, and ABA and ABA)
/// - and a field indicating which index it goes into.
///
/// In practice, it may also contain extra information to help detect other
/// errors (currently it stores a "magic value" used to detect invalid
/// [`Handle`]s).
///
/// These fields may change but the following guarantees are made about the
/// internal representation:
///
/// - This will always be representable in 64 bits.
/// - The bits, when interpreted as a signed 64 bit integer, will be positive
///   (that is to say, it will *actually* be representable in 63 bits, since
///   this makes the most significant bit unavailable for the purposes of
///   encoding). This guarantee makes things slightly less dubious when passing
///   things to Java, gives us some extra validation ability, etc.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Handle {
    map_id: u16,
    version: u16,
    index: u16,
}

// We stuff this into the top 16 bits of the handle when u16 encoded to detect
// various sorts of weirdness. It's the letters 'A' and 'S' as ASCII, but the
// only important thing about it is that the most significant bit be unset.
const HANDLE_MAGIC: u16 = 0x4153_u16;

impl Handle {
    /// Convert a `Handle` to a `u64`. You can also use `Into::into` directly.
    /// Most uses of this will be automatic due to our [`IntoFfi`] implementation.
    #[inline]
    pub fn into_u64(self) -> u64 {
        let map_id = self.map_id as u64;
        let version = self.version as u64;
        let index = self.index as u64;
        // SOMEDAY: we could also use this as a sort of CRC if we were really paranoid.
        // e.g. `magic = combine_to_u16(map_id, version, index)`.
        let magic = HANDLE_MAGIC as u64;
        (magic << 48) | (map_id << 32) | (index << 16) | version
    }

    /// Convert a `u64` to a `Handle`. Inverse of `into_u64`. We also implement
    /// `From::from` (which will panic instead of returning Err).
    ///
    /// Returns [`HandleError::InvalidHandle`](HandleError) if the bits cannot
    /// possibly represent a valid handle.
    pub fn from_u64(v: u64) -> Result<Self, HandleError> {
        if !Handle::is_valid(v) {
            warn!("Illegal handle! {:x}", v);
            Err(HandleError::InvalidHandle)
        } else {
            let map_id = (v >> 32) as u16;
            let index = (v >> 16) as u16;
            let version = v as u16;
            Ok(Self { map_id, version, index })
        }
    }

    /// Returns whether or not `v` makes a bit pattern that could represent an
    /// encoded [`Handle`].
    #[inline]
    pub fn is_valid(v: u64) -> bool {
        (v >> 48) == (HANDLE_MAGIC as u64) &&
        // The "bottom" field is the version. We increment it both when
        // inserting and removing, and they're all initially 1. So, all valid
        // handles that we returned should have an even version.
        ((v & 1) == 0)
    }
}

impl From<u64> for Handle {
    fn from(u: u64) -> Self {
        Handle::from_u64(u).expect("Illegal handle!")
    }
}

impl From<Handle> for u64 {
    #[inline]
    fn from(h: Handle) -> u64 {
        h.into_u64()
    }
}

unsafe impl IntoFfi for Handle {
    type Value = u64;
    // Note: intentionally does not encode a valid handle for any map.
    #[inline] fn ffi_default() -> u64 { 0u64 }
    #[inline] fn into_ffi_value(self) -> u64 { self.into_u64() }
}

// XXX ConcurrentHandleMap is not fully thought out yet.

/// ConcurrentHandleMap is a relatively thin wrapper around
/// `RwLock<HandleMap<Mutex<T>>>`. Due to the nested locking, it's not possible
/// to implement the same API as HandleMap, however it does implement an API
/// that offers equivalent functionality.
pub struct ConcurrentHandleMap<T> {
    pub map: RwLock<HandleMap<Mutex<T>>>,
}

impl<T> ConcurrentHandleMap<T> {
    /// Construct a new `ConcurrentHandleMap`.
    pub fn new() -> Self {
        Self { map: RwLock::new(HandleMap::new()) }
    }

    /// Insert an item into the map.
    pub fn insert(&self, v: T) -> Handle {
        // Fails if the lock is poisoned. Not clear what we should do here... We
        // could always insert anyway (by matching on LockResult), but that
        // seems... really quite dubious.
        let mut map = self.map.write().unwrap();
        map.insert(Mutex::new(v))
    }

    /// Remove an item from the map.
    pub fn delete(&self, h: Handle) -> Result<(), HandleError> {
        // XXX figure out how to handle poison...
        let mut map = self.map.write().unwrap();
        map.delete(h)
    }

    /// Call `callback` with a non-mutable reference to the item from the map,
    /// after acquiring the necessary locks.
    pub fn get<F, E, R>(&self, h: Handle, callback: F) -> Result<R, E>
    where
        F: FnOnce(&T) -> Result<R, E>,
        E: From<HandleError>,
    {
        // XXX figure out how to handle poison...
        let map = self.map.read().unwrap();
        let mtx = map.get(h)?;
        let hm = mtx.lock().unwrap();
        callback(&*hm)
    }

    /// Call `callback` with a mutable reference to the item from the map,
    /// after acquiring the necessary locks.
    pub fn get_mut<F, E, R>(&self, h: Handle, callback: F) -> Result<R, E>
    where
        F: FnOnce(&mut T) -> Result<R, E>,
        E: From<HandleError>,
    {
        // XXX figure out how to handle poison...
        let map = self.map.read().unwrap();
        let mtx = map.get(h)?;
        let mut hm = mtx.lock().unwrap();
        callback(&mut *hm)
    }

    /// Convenient wrapper for `get` which takes a `u64` that it will convert to
    /// a handle.
    pub fn get_u64<F, E, R>(&self, u: u64, callback: F) -> Result<R, E>
    where
        F: FnOnce(&T) -> Result<R, E>,
        E: From<HandleError>,
    {
        self.get(Handle::from_u64(u)?, callback)
    }

    /// Convenient wrapper for `get_mut` which takes a `u64` that it will
    /// convert to a handle.
    pub fn get_mut_u64<F, E, R>(&self, u: u64, callback: F) -> Result<R, E>
    where
        F: FnOnce(&mut T) -> Result<R, E>,
        E: From<HandleError>,
    {
        self.get_mut(Handle::from_u64(u)?, callback)
    }
}

// Returns the next map_id.
fn next_handle_map_id() -> u16 {
    let id = HANDLE_MAP_ID_COUNTER.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    id as u16
}

// Note: These IDs are only used to detect using a key against the wrong HandleMap.
// We ensure they're randomly initialized, to prevent using them across separately
// compiled .so files.
lazy_static! {
    // This should be `AtomicU16`, but those aren't stablilized yet.
    // Instead, we just cast to u16 on read.
    static ref HANDLE_MAP_ID_COUNTER: AtomicUsize = {
        // Abuse HashMap's RandomState to get a strong RNG without bringing in
        // the `rand` crate (OTOH maybe we should just bring in the rand crate?)
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        let init = RandomState::new().build_hasher().finish() as usize;
        AtomicUsize::new(init)
    };
}

#[cfg(test)]
mod test {
    use super::*;

    #[derive(PartialEq, Debug)]
    struct Foobar(usize);

    #[test]
    fn test_invalid_handle() {
        assert_eq!(Handle::from_u64(0), Err(HandleError::InvalidHandle));
        // Valid except `version` is odd
        assert_eq!(Handle::from_u64(((HANDLE_MAGIC as u64) << 48) | 0x1234_0012_0001),
                   Err(HandleError::InvalidHandle));

        assert_eq!(Handle::from_u64(((HANDLE_MAGIC as u64) << 48) | 0x1234_0012_0002), Ok(Handle {
            version: 0x0002,
            index: 0x0012,
            map_id: 0x1234,
        }));
    }

    #[test]
    fn test_correct_value_single() {
        let mut map = HandleMap::new();
        let handle = map.insert(Foobar(1234));
        assert_eq!(map.get(handle).unwrap(), &Foobar(1234));
        map.delete(handle).unwrap();
        assert_eq!(map.get(handle), Err(HandleError::StaleVersion));
    }

    #[test]
    fn test_correct_value_multiple() {
        let mut map = HandleMap::new();
        let handle1 = map.insert(Foobar(1234));
        let handle2 = map.insert(Foobar(4321));
        assert_eq!(map.get(handle1).unwrap(), &Foobar(1234));
        assert_eq!(map.get(handle2).unwrap(), &Foobar(4321));
        map.delete(handle1).unwrap();
        assert_eq!(map.get(handle1), Err(HandleError::StaleVersion));
        assert_eq!(map.get(handle2).unwrap(), &Foobar(4321));
    }

    #[test]
    fn test_wrong_map() {
        let mut map1 = HandleMap::new();
        let mut map2 = HandleMap::new();

        let handle1 = map1.insert(Foobar(1234));
        let handle2 = map2.insert(Foobar(1234));

        assert_eq!(map1.get(handle1).unwrap(), &Foobar(1234));
        assert_eq!(map2.get(handle2).unwrap(), &Foobar(1234));

        assert_eq!(map1.get(handle2), Err(HandleError::WrongMap));
        assert_eq!(map2.get(handle1), Err(HandleError::WrongMap));
    }

    #[test]
    fn test_bad_index() {
        let map: HandleMap<Foobar> = HandleMap::new();
        assert_eq!(map.get(Handle {
            map_id: map.id,
            version: 2,
            index: 100
        }), Err(HandleError::IndexPastEnd));
    }

    #[test]
    fn test_resizing() {
        let mut map = HandleMap::new();
        let mut handles = vec![];
        for i in 0..1000 {
            handles.push(map.insert(Foobar(i)))
        }
        for (i, &h) in handles.iter().enumerate() {
            assert_eq!(map.get(h).unwrap(), &Foobar(i));
            map.delete(h).unwrap();
        }
        let mut handles2 = vec![];
        for i in 1000..2000 {
            // Not really related to this test, but it's convenient to check this here.
            let h = map.insert(Foobar(i));
            let hu = h.into_u64();
            assert_eq!(Handle::from_u64(hu).unwrap(), h);
            handles2.push(hu);
        }

        for (i, (&h0, h1u)) in handles.iter().zip(handles2).enumerate() {
            // It's still a stale version, even though the slot is occupied again.
            assert_eq!(map.get(h0), Err(HandleError::StaleVersion));
            let h1 = Handle::from_u64(h1u).unwrap();
            assert_eq!(map.get(h1).unwrap(), &Foobar(i + 1000));
        }
    }

}
