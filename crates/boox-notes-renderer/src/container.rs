//! ZIP container access for `.note` files.
//!
//! A `.note` file is a ZIP archive (usually stored uncompressed). It contains
//! protobuf metadata, JSON templates and a custom binary stroke format. Some
//! entries (`shape/*.zip`) are themselves nested ZIP archives.
//!
//! Notes are small enough (a few MB) that we eagerly read every entry into
//! memory and index it by name. This keeps the rest of the code simple: lookups
//! are infallible map accesses rather than seek-and-decompress dances.

use std::collections::BTreeMap;
use std::io::{self, Read, Seek};

use zip::ZipArchive;

use crate::error::{Error, Result};

/// All entries of a `.note` archive, indexed by their full path inside the ZIP.
pub struct Container {
    entries: BTreeMap<String, Vec<u8>>,
}

impl Container {
    /// Read every entry of the archive into memory.
    pub fn open<R: Read + Seek>(reader: R) -> Result<Self> {
        // ZIP errors fold into `io::Error` (the `zip` crate provides the
        // conversion) so `zip` stays out of the public error type.
        let mut archive = ZipArchive::new(reader).map_err(|e| Error::Archive(e.into()))?;
        let mut entries = BTreeMap::new();
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).map_err(|e| Error::Archive(e.into()))?;
            if !file.is_file() {
                continue;
            }
            let name = file.name().to_string();
            let mut buf = Vec::with_capacity(file.size() as usize);
            file.read_to_end(&mut buf).map_err(|e| {
                Error::Archive(io::Error::new(e.kind(), format!("ZIP entry {name}: {e}")))
            })?;
            entries.insert(name, buf);
        }
        Ok(Self { entries })
    }

    /// Returns the bytes for an exact entry path, if present.
    pub fn get(&self, path: &str) -> Option<&[u8]> {
        self.entries.get(path).map(|v| v.as_slice())
    }

    /// Returns all entry paths that start with `prefix`.
    pub fn entries_with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a str> {
        self.entries
            .keys()
            .map(String::as_str)
            .filter(move |k| k.starts_with(prefix))
    }

    /// Returns every entry path.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

/// Decode a nested ZIP (e.g. a `shape/*.zip` entry) and return the bytes of its
/// single inner file. Boox stores exactly one protobuf payload per shape ZIP.
/// Failures here are per-entry and recoverable (the caller warns and skips),
/// so this returns a plain `io::Error` rather than `crate::Error`.
pub fn read_nested_zip_single(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let cursor = io::Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor).map_err(io::Error::from)?;
    // `by_index(0)` reports an empty nested ZIP as FileNotFound.
    let mut file = archive.by_index(0).map_err(io::Error::from)?;
    let mut buf = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buf)?;
    Ok(buf)
}
