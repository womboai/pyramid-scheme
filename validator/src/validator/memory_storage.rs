use std::fs::File;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::{fs, io::Result};

use memmap2::{MmapMut, MmapOptions};

pub trait MemoryMapped {
    fn open(path: impl AsRef<Path>, initial_capacity: u64) -> Result<Self>
    where
        Self: Sized;

    fn flush(&mut self) -> Result<()>;

    fn ensure_capacity(&mut self, capacity: u64) -> Result<()>;
}

pub struct MemoryMappedFile {
    mmap: MmapMut,
    file: File,
    path: PathBuf,
}

impl MemoryMappedFile {
    fn new(file: File, path: impl AsRef<Path>) -> Result<Self> {
        // SAFETY: This would typically not be safe as this is technically a self-referential
        //  struct. Mmap is using a reference of `&File` without a lifetime.
        //  However, this works as mmap uses the file descriptor internally,
        //  so even if {File} is moved, the descriptor remains the same,
        //  and the file descriptor is closed at the same time the mmap is closed.

        let mmap = unsafe { MmapOptions::new().map_mut(&file)? };

        Ok(Self {
            mmap,
            file,
            path: path.as_ref().to_path_buf(),
        })
    }
}

impl MemoryMapped for MemoryMappedFile {
    fn open(path: impl AsRef<Path>, capacity: u64) -> Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        file.set_len(capacity)?;

        Self::new(file, path)
    }

    fn flush(&mut self) -> Result<()> {
        self.mmap.flush()
    }

    fn ensure_capacity(&mut self, capacity: u64) -> Result<()> {
        if self.mmap.len() >= capacity as usize {
            return Ok(());
        }

        self.file.set_len(capacity)?;

        self.mmap = unsafe { MmapOptions::new().map_mut(&self.file) }?;

        Ok(())
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

unsafe impl Sync for MemoryMappedFile {}
unsafe impl Send for MemoryMappedFile {}

pub struct MemoryMappedStorage {
    swap_file: MemoryMappedFile,
    storage_path: PathBuf,
}

impl MemoryMapped for MemoryMappedStorage {
    fn open(storage_path: impl AsRef<Path>, initial_capacity: u64) -> Result<Self> {
        let mut swap_path = PathBuf::new();

        swap_path.push(&storage_path);

        let mut file_name = swap_path.file_name().unwrap().to_owned();
        file_name.push(".swap");

        swap_path.pop();
        swap_path.push(file_name);

        if fs::exists(&storage_path)? {
            fs::copy(&storage_path, &swap_path)?;
        }

        let memory_mapped_swap_file = MemoryMappedFile::open(swap_path, initial_capacity).unwrap();

        Ok(Self {
            swap_file: memory_mapped_swap_file,
            storage_path: storage_path.as_ref().to_path_buf(),
        })
    }

    fn flush(&mut self) -> Result<()> {
        self.swap_file.mmap.flush()?;
        fs::copy(&self.swap_file.path, &self.storage_path)?;

        Ok(())
    }

    fn ensure_capacity(&mut self, capacity: u64) -> Result<()> {
        self.swap_file.ensure_capacity(capacity)
    }
}

impl Deref for MemoryMappedStorage {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.swap_file
    }
}

impl DerefMut for MemoryMappedStorage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.swap_file
    }
}

unsafe impl Sync for MemoryMappedStorage {}
unsafe impl Send for MemoryMappedStorage {}
