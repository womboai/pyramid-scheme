use std::{fs, io};
use std::fs::File;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

use anyhow::Result;
use memmap2::{MmapMut, MmapOptions};
use tempfile::NamedTempFile;

pub struct MemoryMappedFile {
    mmap: MmapMut,
    path: PathBuf,
    _file: File,
}

impl MemoryMappedFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::new(File::open(&path)?, path))
    }

    pub fn new(file: File, path: impl AsRef<Path>) -> Self {
        // SAFETY: This would typically not be safe as this is technically a self-referential
        //  struct. Mmap is using a reference of `&file` without a lifetime.
        //  However, this works as mmap uses the file descriptor internally,
        //  so even if {File} is moved, the descriptor remains the same,
        //  and the file descriptor is closed at the same time the mmap is closed.
        let mmap = unsafe { MmapOptions::new().map_mut(&file) }.unwrap();

        Self {
            mmap,
            path: path.as_ref().to_path_buf(),
            _file: file,
        }
    }

    pub fn flush(&self) -> io::Result<()> {
        self.mmap.flush()
    }
}

impl Deref for MemoryMappedFile {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.mmap
    }
}

impl DerefMut for MemoryMappedFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.mmap
    }
}

pub struct MemoryMappedStorage {
    temporary_file: MemoryMappedFile,
    storage_path: PathBuf,
}

impl MemoryMappedStorage {
    pub fn new(storage_path: impl AsRef<Path>) -> Result<Self> {
        let temporary_file_path = NamedTempFile::new()?;

        if fs::exists(&storage_path)? {
            fs::rename(&storage_path, &temporary_file_path)?;
        }

        let memored_mapped_temporary_file = MemoryMappedFile::new(
            temporary_file_path.reopen()?,
            temporary_file_path.path().to_owned(),
        );

        Ok(Self {
            temporary_file: memored_mapped_temporary_file,
            storage_path: storage_path.as_ref().to_path_buf(),
        })
    }

    pub fn flush(&self) -> Result<()> {
        self.temporary_file.mmap.flush()?;
        fs::rename(&self.temporary_file.path, &self.storage_path)?;

        Ok(())
    }
}

impl Deref for MemoryMappedStorage {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.temporary_file
    }
}

impl DerefMut for MemoryMappedStorage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.temporary_file
    }
}
