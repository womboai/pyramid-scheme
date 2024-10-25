use std::{fs, io};
use std::fs::File;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

use anyhow::Result;
use memmap2::{MmapMut, MmapOptions};

pub struct MemoryMappedFile {
    mmap: MmapMut,
    path: PathBuf,
    _file: File,
}

impl MemoryMappedFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        Ok(Self::new(file, path))
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
    swap_file: MemoryMappedFile,
    storage_path: PathBuf,
}

impl MemoryMappedStorage {
    pub fn new(storage_path: impl AsRef<Path>) -> Result<Self> {
        let mut swap_path = PathBuf::new();

        swap_path.push(&storage_path);

        let mut file_name = swap_path.file_name().unwrap().to_owned();
        file_name.push(".swap");

        swap_path.pop();
        swap_path.push(file_name);

        if fs::exists(&storage_path)? {
            fs::rename(&storage_path, &swap_path)?;
        }

        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open(&swap_path)?;

        let memory_mapped_swap_file = MemoryMappedFile::new(file, swap_path);

        Ok(Self {
            swap_file: memory_mapped_swap_file,
            storage_path: storage_path.as_ref().to_path_buf(),
        })
    }

    pub fn flush(&self) -> Result<()> {
        self.swap_file.mmap.flush()?;
        fs::rename(&self.swap_file.path, &self.storage_path)?;

        Ok(())
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
