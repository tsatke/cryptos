use alloc::string::String;
use alloc::vec::Vec;
use core::convert::TryInto;
use core::hash::{BuildHasherDefault, Hasher};
use sha3::{Digest, Sha3_512};
use unix_path::PathBuf;

// return the first 64 bits of a 512-bit hash
pub fn u64_from_slice(slice: &mut [u8]) -> u64 {
    u64::from_ne_bytes(slice.split_at_mut(8).0.try_into().unwrap())
}

// need something far more secure than AHash here to pave the way for things like per-directory encryption
pub struct HMFSHasher(Sha3_512);

impl Hasher for HMFSHasher {
    fn finish(&self) -> u64 {
        u64_from_slice(self.0.clone().finalize().as_mut_slice())
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes)
    }
}

pub type HMFSHashBuilder = BuildHasherDefault<HMFSHasher>;
pub type HashMap<K, V> = hashbrown::HashMap<K, V, HMFSHashBuilder>;

// going one-further than most other implementations to ensure this never overflows
#[allow(non_camel_case_types)]
pub type time_t = u128;

pub type FileData = Vec<u8>;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum EntryKind {
    Directory(*mut HashMap<Properties, *mut Entry>),
    File(FileData),
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Entry(EntryKind);

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Properties {
    name: String,
    entry_kind: EntryKind,
    mime_type: Option<String>,
    mode: u32,
    created_by: String,
    date_created: time_t,
    date_modified: time_t,
    owner: String,
}

// Needed to allow writing HashMaps directly to the disk
pub fn hashmap_bytes<K, V>(map: HashMap<K, V>) -> &'static mut [u8] {
    let map_addr = &map as *const _ as usize as u64;
    unsafe {
        core::slice::from_raw_parts_mut(map_addr as *mut u8, core::mem::size_of::<HashMap<K, V>>())
    }
}
