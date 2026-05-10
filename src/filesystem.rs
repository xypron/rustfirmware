//! Filesystem abstractions shared by FAT and future formats.
//!
//! This module defines the filesystem-independent file operations that higher
//! layers need when opening, inspecting, and loading files from different
//! on-disk formats.

use crate::memory::{EFI_PHYSICAL_ADDRESS, PageAllocator};
use core::slice;

/// Type of object referenced by one filesystem path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileType {
    /// Regular file containing loadable bytes.
    File,
    /// Directory containing child entries.
    Directory,
}

/// Filesystem-independent metadata for one opened path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileInfo {
    /// Whether the path resolves to a file or a directory.
    file_type: FileType,
    /// Size in bytes associated with the opened path.
    size_bytes: usize,
}

impl FileInfo {
    /// Creates one filesystem-independent file metadata record.
    ///
    /// # Parameters
    ///
    /// - `file_type`: Whether the path is a file or a directory.
    /// - `size_bytes`: Size in bytes associated with the path.
    pub const fn new(file_type: FileType, size_bytes: usize) -> Self {
        Self {
            file_type,
            size_bytes,
        }
    }
}

/// Shared metadata operations supported by opened filesystem paths.
pub trait FileInfoView {
    /// Returns whether the opened path is a file or a directory.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn file_type(&self) -> FileType;

    /// Returns the size in bytes associated with the opened path.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn size_bytes(&self) -> usize;

    /// Returns a detached metadata snapshot for the opened path.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn info(&self) -> FileInfo {
        FileInfo::new(self.file_type(), self.size_bytes())
    }
}

impl FileInfoView for FileInfo {
    /// Returns whether the described path is a file or a directory.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn file_type(&self) -> FileType {
        self.file_type
    }

    /// Returns the recorded size in bytes.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn size_bytes(&self) -> usize {
        self.size_bytes
    }
}

/// File contents loaded into EFI-style pages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadedFile {
    /// Page-aligned physical start address of the allocation.
    physical_start: EFI_PHYSICAL_ADDRESS,
    /// Number of 4 KiB pages backing the allocation.
    page_count: usize,
    /// Exact file size copied into the allocation.
    size_bytes: usize,
}

impl LoadedFile {
    /// Creates metadata for one page-backed loaded file.
    ///
    /// # Parameters
    ///
    /// - `physical_start`: Page-aligned physical start address.
    /// - `page_count`: Number of allocated 4 KiB pages.
    /// - `size_bytes`: Exact file size copied into the allocation.
    pub const fn new(
        physical_start: EFI_PHYSICAL_ADDRESS,
        page_count: usize,
        size_bytes: usize,
    ) -> Self {
        Self {
            physical_start,
            page_count,
            size_bytes,
        }
    }

    /// Returns the page-aligned physical start address of the allocation.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn physical_start(&self) -> EFI_PHYSICAL_ADDRESS {
        self.physical_start
    }

    /// Returns the number of allocated 4 KiB pages.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn page_count(&self) -> usize {
        self.page_count
    }

    /// Returns the exact file size stored in the allocation.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn size_bytes(&self) -> usize {
        self.size_bytes
    }

    /// Returns the loaded file contents as a byte slice.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn bytes(&self) -> &[u8] {
        unsafe {
            slice::from_raw_parts(
                self.physical_start as *const u8,
                self.size_bytes,
            )
        }
    }
}

/// Filesystem-independent handle for one opened file or directory.
pub trait FileHandle: FileInfoView {
    /// Error type returned by this filesystem implementation.
    type Error;

    /// Loads the file into page-aligned EFI-style memory.
    ///
    /// # Parameters
    ///
    /// - `allocator`: Page allocator used to reserve the destination pages.
    fn load(
        &mut self,
        allocator: &mut PageAllocator<'_>,
    ) -> Result<LoadedFile, Self::Error>;

    /// Loads the file into page-aligned EFI-style memory at one fixed address.
    ///
    /// # Parameters
    ///
    /// - `allocator`: Page allocator used to reserve the destination pages.
    /// - `physical_start`: Page-aligned physical start address to allocate.
    fn load_at(
        &mut self,
        allocator: &mut PageAllocator<'_>,
        physical_start: EFI_PHYSICAL_ADDRESS,
    ) -> Result<LoadedFile, Self::Error>;

}

/// Filesystem that can open path-based file handles.
pub trait FileSystem {
    /// Error type returned by this filesystem implementation.
    type Error;
    /// File handle type returned by `open`.
    type File<'file>: FileHandle<Error = Self::Error>
    where
        Self: 'file;

    /// Opens one path as a filesystem file handle.
    ///
    /// # Parameters
    ///
    /// - `path`: Absolute or relative path inside the mounted filesystem.
    fn open<'file>(
        &'file mut self,
        path: &str,
    ) -> Result<Self::File<'file>, Self::Error>;
}

/// Loads the first successfully opened file from one filesystem.
///
/// # Parameters
///
/// - `volume`: Mounted filesystem used to open candidate paths.
/// - `candidate_path`: Returns the next candidate path for one index.
/// - `allocator`: Page allocator used to reserve the destination pages.
/// - `filesystem_name`: Filesystem label used in the load log.
pub fn load_first_file<F: FileSystem, P: AsRef<str> + Copy>(
    volume: &mut F,
    candidate_path: fn(usize) -> Option<P>,
    allocator: &mut PageAllocator<'_>,
    filesystem_name: &str,
) -> Option<(P, LoadedFile)> {
    let mut index = 0usize;
    while let Some(path) = candidate_path(index) {
        let path_text = path.as_ref();
        if let Ok(mut file) = volume.open(path_text) {
            if let Ok(loaded) = file.load(allocator) {
                print_loaded_file(filesystem_name, path_text, &loaded);
                return Some((path, loaded));
            }
        }
        index += 1;
    }

    None
}

/// Prints one loaded file path plus size with a filesystem prefix.
///
/// # Parameters
///
/// - `filesystem_name`: Filesystem label shown before the file path.
/// - `path`: Absolute path of the loaded file.
/// - `loaded_file`: Loaded file metadata including physical address and size.
pub fn print_loaded_file(
    filesystem_name: &str,
    path: &str,
    loaded_file: &LoadedFile,
) {
    crate::println!(
        "{}: loaded '{}', size={} @ {:#018x}",
        filesystem_name,
        path,
        loaded_file.size_bytes(),
        loaded_file.physical_start() as usize,
    );
}