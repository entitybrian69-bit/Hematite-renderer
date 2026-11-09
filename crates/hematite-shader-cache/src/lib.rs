//! Content-addressed cache records for translated shaders and driver pipeline blobs.
//!
//! Records are versioned, length-bounded and CRC32-checked; invalid records are misses.
//! Keys must include source, options, translator version and (for driver-specific data)
//! the device/driver identity. `Key::from_fields` length-prefixes each field so distinct
//! option tuples cannot alias by concatenation. No shader compiler is included here.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod hash;
pub use hash::sha256;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 8] = b"HEMTCACH";
const VERSION: u32 = 1;
const HEADER: usize = 56;
static NONCE: AtomicU64 = AtomicU64::new(0);

/// Full SHA-256 content key. Includes a domain separator and length-prefixed fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key(pub [u8; 32]);

impl Key {
    /// Ordered fields; callers must sort any map-like options before supplying them.
    pub fn from_fields(fields: &[&[u8]]) -> Self {
        let mut hash = hash::Sha256::new();
        hash.update(b"hematite-cache-key-v1\0");
        hash.update(&(fields.len() as u64).to_le_bytes());
        for field in fields {
            hash.update(&(field.len() as u64).to_le_bytes());
            hash.update(field);
        }
        Self(hash.finish())
    }

    /// Lowercase hex filename (no user-controlled path components).
    pub fn filename(self) -> String {
        let mut out = String::with_capacity(68);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in self.0 {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 15) as usize] as char);
        }
        out.push_str(".bin");
        out
    }
}

/// IEEE CRC32, detecting accidental cache corruption (not malicious modification).
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

/// Encode one record; rejects over-limit payloads before allocation.
pub fn encode(key: Key, payload: &[u8], max_payload: usize) -> io::Result<Vec<u8>> {
    if payload.len() > max_payload {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "cache payload exceeds limit"));
    }
    let size = HEADER
        .checked_add(payload.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cache record size overflow"))?;
    let mut out = Vec::new();
    out.try_reserve_exact(size)
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "cache allocation failed"))?;
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&key.0);
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Validate a record and borrow its payload. Any mismatch is a cache miss, never a panic.
/// This validates record integrity, not SPIR-V or driver-specific payload semantics.
pub fn decode(record: &[u8], key: Key, max_payload: usize) -> Option<&[u8]> {
    if record.len() < HEADER
        || &record[..8] != MAGIC
        || record[8..12] != VERSION.to_le_bytes()
        || record[12..44] != key.0
    {
        return None;
    }
    let size = u64::from_le_bytes(record[44..52].try_into().ok()?);
    let size = usize::try_from(size).ok()?;
    if size > max_payload || size.checked_add(HEADER)? != record.len() {
        return None;
    }
    let expected_crc = u32::from_le_bytes(record[52..56].try_into().ok()?);
    let payload = &record[HEADER..];
    (crc32(payload) == expected_crc).then_some(payload)
}

/// A cache in an application-private directory. No eviction policy is included yet.
/// Readers see complete old or new records on Unix/Android because writes use rename.
/// The containing directory must not be writable by an untrusted process (symlink attacks
/// are outside this API's contract). CRC32 is not authentication.
#[derive(Debug)]
pub struct DiskCache {
    root: PathBuf,
    max_payload: usize,
}

impl DiskCache {
    /// Create the cache directory. The size limit bounds each record, not total disk usage.
    pub fn new(root: impl AsRef<Path>, max_payload: usize) -> io::Result<Self> {
        if max_payload.checked_add(HEADER + 1).is_none() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cache limit overflows"));
        }
        fs::create_dir_all(root.as_ref())?;
        Ok(Self { root: root.as_ref().to_path_buf(), max_payload })
    }

    /// Read a record; absent, stale, oversized and corrupted files are cache misses.
    /// Other IO failures are returned to the caller, never silently treated as success.
    pub fn get(&self, key: Key) -> io::Result<Option<Vec<u8>>> {
        let file = match File::open(self.root.join(key.filename())) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let limit = self.max_payload + HEADER;
        if file.metadata()?.len() > limit as u64 {
            return Ok(None);
        }
        let mut record = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut record)?;
        Ok(decode(&record, key, self.max_payload).map(<[u8]>::to_vec))
    }

    /// Persist a complete record with a same-directory, create-new temp file + rename.
    /// The file is synced before rename; directory metadata is not fsynced, so power loss
    /// can lose a cache entry. Killed writers can leave temp files, never partial records.
    pub fn put(&self, key: Key, payload: &[u8]) -> io::Result<()> {
        let record = encode(key, payload, self.max_payload)?;
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let mut candidate = None;
        for _ in 0..16 {
            let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
            let path = self.root.join(format!(".tmp-{timestamp:x}-{nonce:x}"));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    candidate = Some((path, file));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        let (temporary, mut file) = candidate.ok_or_else(|| {
            io::Error::new(io::ErrorKind::AlreadyExists, "cache temp-name collisions")
        })?;
        let result = (|| {
            file.write_all(&record)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, self.root.join(key.filename()))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key::from_fields(&[b"shader", b"compiler-v1", b"options", b"device/driver"])
    }

    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xcbf43926);
    }

    #[test]
    fn field_boundaries_and_identity_affect_keys() {
        assert_ne!(Key::from_fields(&[b"ab", b"c"]), Key::from_fields(&[b"a", b"bc"]));
        assert_ne!(Key::from_fields(&[b""]), Key::from_fields(&[]));
        assert_ne!(
            Key::from_fields(&[b"source", b"driver1"]),
            Key::from_fields(&[b"source", b"driver2"])
        );
        assert_eq!(key(), key());
        assert_eq!(key().filename().len(), 68);
        assert!(key().filename().ends_with(".bin"));
    }

    #[test]
    fn round_trip_and_empty_payload() {
        for payload in [b"".as_slice(), b"SPIR-V bytes".as_slice()] {
            let record = encode(key(), payload, 100).unwrap();
            assert_eq!(decode(&record, key(), 100), Some(payload));
        }
    }

    #[test]
    fn every_truncation_and_single_bit_corruption_is_miss() {
        let record = encode(key(), b"synthetic shader bytes", 100).unwrap();
        for length in 0..record.len() {
            assert!(decode(&record[..length], key(), 100).is_none());
        }
        for index in 0..record.len() {
            let mut corrupt = record.clone();
            corrupt[index] ^= 1;
            assert!(decode(&corrupt, key(), 100).is_none(), "byte {index}");
        }
    }

    #[test]
    fn wrong_key_version_length_and_limit_are_misses() {
        let record = encode(key(), b"test", 100).unwrap();
        assert!(decode(&record, Key([0; 32]), 100).is_none());
        assert!(decode(&record, key(), 3).is_none());
        let mut extra = record.clone();
        extra.push(0);
        assert!(decode(&extra, key(), 100).is_none());
        let mut huge = record;
        huge[44..52].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode(&huge, key(), usize::MAX).is_none());
        assert!(encode(key(), b"test", 3).is_err());
    }

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!("hematite-test-{timestamp:x}-{nonce:x}"));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn disk_round_trip_replacement_and_corrupt_miss() {
        let dir = TestDir::new();
        let cache = DiskCache::new(&dir.0, 1024).unwrap();
        assert_eq!(cache.get(key()).unwrap(), None);
        cache.put(key(), b"first").unwrap();
        assert_eq!(cache.get(key()).unwrap().as_deref(), Some(b"first".as_slice()));
        cache.put(key(), b"second").unwrap();
        assert_eq!(cache.get(key()).unwrap().as_deref(), Some(b"second".as_slice()));
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
        fs::write(dir.0.join(key().filename()), b"corrupt").unwrap();
        assert_eq!(cache.get(key()).unwrap(), None);
    }

    #[test]
    fn oversized_record_is_miss_and_put_rejected() {
        let dir = TestDir::new();
        let cache = DiskCache::new(&dir.0, 2).unwrap();
        assert!(cache.put(key(), b"too big").is_err());
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
        fs::write(dir.0.join(key().filename()), [0u8; 100]).unwrap();
        assert_eq!(cache.get(key()).unwrap(), None);
        assert!(DiskCache::new(&dir.0, usize::MAX).is_err());
    }
}
