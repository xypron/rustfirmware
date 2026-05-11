//! Read-only ext4 filesystem support.
//!
//! This module mounts one ext4 volume from a GPT partition start sector,
//! resolves absolute paths by walking directory entries, follows fast and
//! extent-backed symlinks, and reads regular-file contents through extent
//! mappings into caller-provided buffers or EFI-style page allocations.
//! Parent-directory path components (`..`) are rejected during resolution.

use core::cmp::{max, min};
use core::{ptr, slice, str};

use crate::filesystem::{
    FileHandle, FileInfoView, FileSystem, FileType, LoadedFile,
};
use crate::memory::{
    EFI_ALLOCATE_TYPE, EFI_MEMORY_TYPE, EFI_PAGE_SIZE,
    EFI_PHYSICAL_ADDRESS,
    MemoryError, PageAllocator,
};
use crate::virtio::{BlockDevice, VirtioError, VIRTIO_SECTOR_SIZE};

/// ext4 superblock magic value.
const EXT4_SUPER_MAGIC: u16 = 0xef53;
/// Maximum ext4 block size supported by this implementation.
const EXT4_MAX_BLOCK_SIZE: usize = 4096;
/// Maximum ext4 inode size supported by this implementation.
const EXT4_MAX_INODE_SIZE: usize = 512;
/// Maximum absolute or symlink-expanded path length supported in memory.
const EXT4_PATH_BYTES: usize = 1024;
/// Root inode number used by ext filesystems.
const EXT4_ROOT_INODE: u32 = 2;
/// Maximum symlink expansions allowed while resolving one path.
const EXT4_MAX_SYMLINK_DEPTH: usize = 8;
/// Maximum number of extents collected for one inode traversal.
const EXT4_MAX_EXTENTS: usize = 128;
/// Maximum depth supported by ext4 extent trees.
const EXT4_MAX_EXTENT_DEPTH: u16 = 5;
/// Extent header magic value.
const EXT4_EXTENT_MAGIC: u16 = 0xf30a;
/// Incompatible feature flag for directory entry file types.
const EXT4_FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002;
/// Incompatible feature flag indicating the filesystem needs journal recovery.
const EXT4_FEATURE_INCOMPAT_RECOVER: u32 = 0x0004;
/// Incompatible feature flag for extent-based data mappings.
const EXT4_FEATURE_INCOMPAT_EXTENTS: u32 = 0x0040;
/// Incompatible feature flag for 64-bit block-group metadata.
const EXT4_FEATURE_INCOMPAT_64BIT: u32 = 0x0080;
/// Incompatible feature flag for flex block groups.
const EXT4_FEATURE_INCOMPAT_FLEX_BG: u32 = 0x0200;
/// Bitmask of incompatible features supported by this implementation.
const EXT4_SUPPORTED_INCOMPAT_FEATURES: u32 =
    EXT4_FEATURE_INCOMPAT_FILETYPE
    | EXT4_FEATURE_INCOMPAT_RECOVER
    | EXT4_FEATURE_INCOMPAT_EXTENTS
    | EXT4_FEATURE_INCOMPAT_64BIT
    | EXT4_FEATURE_INCOMPAT_FLEX_BG;
/// Inode flag indicating that `i_block` stores one extent tree root.
const EXT4_INODE_FLAG_EXTENTS: u32 = 0x0008_0000;
/// File-type bits identifying one regular file.
const EXT4_MODE_REGULAR: u16 = 0x8000;
/// File-type bits identifying one directory.
const EXT4_MODE_DIRECTORY: u16 = 0x4000;
/// File-type bits identifying one symlink.
const EXT4_MODE_SYMLINK: u16 = 0xa000;
/// File-type mask extracted from the inode mode field.
const EXT4_MODE_TYPE_MASK: u16 = 0xf000;

/// Errors returned by the ext4 filesystem reader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ext4Error {
    /// The underlying block device reported an I/O failure.
    Device(VirtioError),
    /// The EFI-style page allocator reported an allocation failure.
    Memory(MemoryError),
    /// The superblock did not describe a supported ext filesystem.
    InvalidSuperblock,
    /// The ext volume uses a block size larger than this implementation supports.
    UnsupportedBlockSize(u32),
    /// The ext volume uses an inode size larger than this implementation supports.
    UnsupportedInodeSize(u16),
    /// The ext volume advertises incompatible features this reader does not understand.
    UnsupportedIncompatibleFeatures(u32),
    /// The inode uses one mapping mode this implementation does not support.
    UnsupportedMapping,
    /// The requested path was empty or otherwise malformed.
    InvalidPath,
    /// A required path component or file was not found.
    NotFound,
    /// A path component that should have been a directory was not one.
    NotDirectory,
    /// The resolved path points at a directory instead of a regular file.
    IsDirectory,
    /// The caller-supplied output buffer is too small for the file.
    BufferTooSmall,
    /// The filesystem referenced an invalid inode number or layout.
    InvalidInode(u32),
    /// The filesystem contained one invalid extent tree.
    InvalidExtentTree,
    /// The filesystem contained one malformed directory entry stream.
    InvalidDirectoryEntry,
    /// One symlink target exceeded the supported in-memory path limit.
    NameTooLong,
    /// Too many symlink expansions were required while resolving one path.
    SymlinkLoop,
    /// The resolved inode type is unsupported by this implementation.
    UnsupportedFileType(u16),
}

impl From<VirtioError> for Ext4Error {
    /// Converts one block-device error into the matching ext4-layer error.
    ///
    /// # Parameters
    ///
    /// - `error`: Block-device error to wrap.
    fn from(error: VirtioError) -> Self {
        Self::Device(error)
    }
}

impl From<MemoryError> for Ext4Error {
    /// Converts one page-allocation error into the matching ext4-layer error.
    ///
    /// # Parameters
    ///
    /// - `error`: Page-allocation error to wrap.
    fn from(error: MemoryError) -> Self {
        Self::Memory(error)
    }
}

/// Mounted read-only ext4 volume backed by a block device.
pub struct Ext4Volume<'a, D: BlockDevice> {
    /// Underlying sector-addressable block device.
    device: &'a mut D,
    /// Partition-relative start sector of the mounted volume.
    partition_start_lba: u64,
    /// Logical ext4 block size in bytes.
    block_size: usize,
    /// Number of 512-byte sectors covered by one ext4 block.
    sectors_per_block: u64,
    /// Number of inodes stored in one block group.
    inodes_per_group: u32,
    /// Size in bytes of one on-disk inode record.
    inode_size: u16,
    /// Size in bytes of one group-descriptor record.
    group_descriptor_size: u16,
    /// Block number containing the first group descriptor.
    group_descriptor_table_block: u64,
}

/// Parsed inode metadata needed by this implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Ext4Inode {
    /// Inode number used to re-read this inode later.
    inode_number: u32,
    /// Raw mode bits that encode file type and permissions.
    mode: u16,
    /// Logical file size in bytes.
    size_bytes: u64,
    /// Raw inode flags field.
    flags: u32,
    /// Inline `i_block` payload used for extents or fast symlinks.
    block_data: [u8; 60],
}

/// Parsed directory entry resolved while traversing one path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Ext4DirectoryEntry {
    /// Inode number referenced by the directory entry.
    inode_number: u32,
}

/// One decoded extent mapping from logical file blocks to physical disk blocks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Ext4Extent {
    /// First logical file block covered by this extent.
    logical_block: u32,
    /// Number of initialized blocks covered by this extent.
    block_count: u16,
    /// First physical filesystem block backing this extent.
    physical_block: u64,
}

/// Open ext4 path handle used to inspect metadata and load contents.
pub struct Ext4File<'volume, 'device, D: BlockDevice> {
    /// Mounted volume that owns the file contents.
    volume: &'volume mut Ext4Volume<'device, D>,
    /// Resolved inode metadata for the opened path.
    inode: Ext4Inode,
}

impl Ext4Inode {
    /// Returns `true` when the inode describes one directory.
    fn is_directory(&self) -> bool {
        (self.mode & EXT4_MODE_TYPE_MASK) == EXT4_MODE_DIRECTORY
    }

    /// Returns `true` when the inode describes one regular file.
    fn is_regular_file(&self) -> bool {
        (self.mode & EXT4_MODE_TYPE_MASK) == EXT4_MODE_REGULAR
    }

    /// Returns `true` when the inode describes one symlink.
    fn is_symlink(&self) -> bool {
        (self.mode & EXT4_MODE_TYPE_MASK) == EXT4_MODE_SYMLINK
    }

    /// Returns `true` when the inode stores data via extents.
    fn uses_extents(&self) -> bool {
        (self.flags & EXT4_INODE_FLAG_EXTENTS) != 0
    }

    /// Returns the external file type represented by this inode.
    fn file_type(&self) -> Result<FileType, Ext4Error> {
        if self.is_directory() {
            Ok(FileType::Directory)
        } else if self.is_regular_file() {
            Ok(FileType::File)
        } else {
            Err(Ext4Error::UnsupportedFileType(self.mode))
        }
    }
}

impl<'volume, 'device, D: BlockDevice> FileInfoView for Ext4File<'volume, 'device, D> {
    /// Returns whether the opened path is a file or a directory.
    fn file_type(&self) -> FileType {
        self.inode.file_type().unwrap_or(FileType::File)
    }

    /// Returns the size in bytes associated with the opened path.
    fn size_bytes(&self) -> usize {
        usize::try_from(self.inode.size_bytes).unwrap_or(usize::MAX)
    }
}

impl<'volume, 'device, D: BlockDevice> FileHandle for Ext4File<'volume, 'device, D> {
    type Error = Ext4Error;

    /// Loads the file into page-aligned EFI-style memory.
    ///
    /// # Parameters
    ///
    /// - `allocator`: Page allocator used to reserve the destination pages.
    fn load(
        &mut self,
        allocator: &mut PageAllocator<'_>,
    ) -> Result<LoadedFile, Ext4Error> {
        self.load_into_allocated_pages(allocator)
    }

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
    ) -> Result<LoadedFile, Ext4Error> {
        self.load_into_pages(
            allocator,
            EFI_ALLOCATE_TYPE::AllocateAddress,
            physical_start,
        )
    }
}

impl<'volume, 'device, D: BlockDevice> Ext4File<'volume, 'device, D> {
    /// Loads the resolved regular file into allocator-chosen EFI-style pages.
    ///
    /// # Parameters
    ///
    /// - `allocator`: Page allocator used to reserve the destination pages.
    fn load_into_allocated_pages(
        &mut self,
        allocator: &mut PageAllocator<'_>,
    ) -> Result<LoadedFile, Ext4Error> {
        if self.inode.is_directory() {
            return Err(Ext4Error::IsDirectory);
        }

        let size_bytes = usize::try_from(self.inode.size_bytes)
            .map_err(|_| Ext4Error::BufferTooSmall)?;
        let page_count = max(1, file_size_to_page_count(size_bytes)?);
        let physical_start = allocator.allocate_pages_for_size(
            EFI_MEMORY_TYPE::EfiBootServicesData,
            size_bytes,
        )?;

        let allocation_size = page_count * EFI_PAGE_SIZE as usize;
        let buffer = unsafe {
            slice::from_raw_parts_mut(
                physical_start as *mut u8,
                allocation_size,
            )
        };
        unsafe {
            ptr::write_bytes(buffer.as_mut_ptr(), 0, buffer.len());
        }

        self.volume.read_inode_contents(
            &self.inode,
            &mut buffer[..size_bytes],
        )?;

        Ok(LoadedFile::new(physical_start, page_count, size_bytes))
    }

    /// Loads the resolved regular file into EFI-style pages.
    ///
    /// # Parameters
    ///
    /// - `allocator`: Page allocator used to reserve the destination pages.
    /// - `allocation_type`: EFI allocation policy to apply.
    /// - `requested_start`: Requested physical start address when using fixed allocation.
    fn load_into_pages(
        &mut self,
        allocator: &mut PageAllocator<'_>,
        allocation_type: EFI_ALLOCATE_TYPE,
        requested_start: EFI_PHYSICAL_ADDRESS,
    ) -> Result<LoadedFile, Ext4Error> {
        if self.inode.is_directory() {
            return Err(Ext4Error::IsDirectory);
        }

        let size_bytes = usize::try_from(self.inode.size_bytes)
            .map_err(|_| Ext4Error::BufferTooSmall)?;
        let page_count = max(1, file_size_to_page_count(size_bytes)?);
        let mut physical_start = requested_start;
        allocator.AllocatePages(
            allocation_type,
            EFI_MEMORY_TYPE::EfiBootServicesData,
            page_count,
            &mut physical_start,
        )?;

        let allocation_size = page_count * EFI_PAGE_SIZE as usize;
        let buffer = unsafe {
            slice::from_raw_parts_mut(
                physical_start as *mut u8,
                allocation_size,
            )
        };
        unsafe {
            ptr::write_bytes(buffer.as_mut_ptr(), 0, buffer.len());
        }

        self.volume.read_inode_contents(
            &self.inode,
            &mut buffer[..size_bytes],
        )?;

        Ok(LoadedFile::new(physical_start, page_count, size_bytes))
    }
}

impl<'a, D: BlockDevice> FileSystem for Ext4Volume<'a, D> {
    type Error = Ext4Error;
    type File<'file>
        = Ext4File<'file, 'a, D>
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
    ) -> Result<Self::File<'file>, Self::Error> {
        let (_, inode) = self.resolve_path(path, 0)?;
        Ok(Ext4File { volume: self, inode })
    }
}

impl<'a, D: BlockDevice> Ext4Volume<'a, D> {
    /// Mounts one ext4 filesystem from a partition start sector.
    ///
    /// # Parameters
    ///
    /// - `device`: Underlying block device that backs the ext4 volume.
    /// - `partition_start_lba`: First sector of the filesystem partition.
    pub fn new(
        device: &'a mut D,
        partition_start_lba: u64,
    ) -> Result<Self, Ext4Error> {
        let mut superblock_bytes = [0u8; 1024];
        read_device_bytes(
            device,
            partition_start_lba,
            1024,
            &mut superblock_bytes,
        )?;

        let magic = read_u16(&superblock_bytes, 0x38)
            .ok_or(Ext4Error::InvalidSuperblock)?;
        if magic != EXT4_SUPER_MAGIC {
            return Err(Ext4Error::InvalidSuperblock);
        }

        let log_block_size = read_u32(&superblock_bytes, 0x18)
            .ok_or(Ext4Error::InvalidSuperblock)?;
        let block_size = 1024u32
            .checked_shl(log_block_size)
            .ok_or(Ext4Error::InvalidSuperblock)?;
        if block_size as usize > EXT4_MAX_BLOCK_SIZE
            || block_size < VIRTIO_SECTOR_SIZE as u32
            || !(block_size as usize).is_multiple_of(VIRTIO_SECTOR_SIZE)
        {
            return Err(Ext4Error::UnsupportedBlockSize(block_size));
        }

        let inode_size = read_u16(&superblock_bytes, 0x58)
            .ok_or(Ext4Error::InvalidSuperblock)?;
        if inode_size == 0 || inode_size as usize > EXT4_MAX_INODE_SIZE {
            return Err(Ext4Error::UnsupportedInodeSize(inode_size));
        }

        let incompat_features = read_u32(&superblock_bytes, 0x60)
            .ok_or(Ext4Error::InvalidSuperblock)?;
        let unsupported = incompat_features & !EXT4_SUPPORTED_INCOMPAT_FEATURES;
        if unsupported != 0 {
            return Err(Ext4Error::UnsupportedIncompatibleFeatures(unsupported));
        }

        let blocks_per_group = read_u32(&superblock_bytes, 0x20)
            .ok_or(Ext4Error::InvalidSuperblock)?;
        let inodes_per_group = read_u32(&superblock_bytes, 0x28)
            .ok_or(Ext4Error::InvalidSuperblock)?;
        if blocks_per_group == 0 || inodes_per_group == 0 {
            return Err(Ext4Error::InvalidSuperblock);
        }

        let group_descriptor_size = read_u16(&superblock_bytes, 0xfe)
            .ok_or(Ext4Error::InvalidSuperblock)?;
        let group_descriptor_size = if group_descriptor_size == 0 {
            32
        } else {
            group_descriptor_size
        };
        if group_descriptor_size != 32 && group_descriptor_size != 64 {
            return Err(Ext4Error::InvalidSuperblock);
        }

        let group_descriptor_table_block = if block_size == 1024 { 2 } else { 1 };

        Ok(Self {
            device,
            partition_start_lba,
            block_size: block_size as usize,
            sectors_per_block: u64::from(block_size) / VIRTIO_SECTOR_SIZE as u64,
            inodes_per_group,
            inode_size,
            group_descriptor_size,
            group_descriptor_table_block,
        })
    }

    /// Walks all files contained below one directory path.
    ///
    /// Symlinks are reported to `visitor` just like regular file-like entries.
    /// The reported size is the inode byte size, which for symlinks is the
    /// target-path length rather than the size of the resolved target.
    /// Parent-directory path components (`..`) are not supported while
    /// resolving `path`.
    ///
    /// # Parameters
    ///
    /// - `path`: Absolute or relative path of the directory to enumerate.
    /// - `visitor`: Callback invoked once per discovered file-like entry.
    pub fn walk_files_in_directory<F>(
        &mut self,
        path: &str,
        mut visitor: F,
    ) -> Result<(), Ext4Error>
    where
        F: FnMut(&str, u64),
    {
        let (_, inode) = self.resolve_path(path, 0)?;
        if !inode.is_directory() {
            return Err(Ext4Error::NotDirectory);
        }

        let trimmed_path = trim_leading_separators(path);
        let mut current_path = [0u8; EXT4_PATH_BYTES];
        let mut path_len = 0usize;
        if !trimmed_path.is_empty() {
            let initial_len = trimmed_path.len() + 1;
            if initial_len > current_path.len() {
                return Err(Ext4Error::NameTooLong);
            }

            current_path[0] = b'/';
            current_path[1..initial_len]
                .copy_from_slice(trimmed_path.as_bytes());
            path_len = initial_len;
        }

        self.walk_directory(&inode, &mut current_path, path_len, &mut visitor)
    }

    /// Resolves one absolute or relative path to its final inode.
    ///
    /// # Parameters
    ///
    /// - `path`: Absolute or relative ext4 path.
    /// - `symlink_depth`: Number of symlink expansions already performed.
    fn resolve_path(
        &mut self,
        path: &str,
        symlink_depth: usize,
    ) -> Result<(u32, Ext4Inode), Ext4Error> {
        self.resolve_path_from(EXT4_ROOT_INODE, path, symlink_depth)
    }

    /// Resolves one path relative to `start_inode_number`.
    ///
    /// # Parameters
    ///
    /// - `start_inode_number`: Directory inode where relative lookup begins.
    /// - `path`: Absolute or relative ext4 path. Parent-directory components
    ///   (`..`) are rejected.
    /// - `symlink_depth`: Number of symlink expansions already performed.
    fn resolve_path_from(
        &mut self,
        start_inode_number: u32,
        path: &str,
        symlink_depth: usize,
    ) -> Result<(u32, Ext4Inode), Ext4Error> {
        let mut current_inode_number = if path.as_bytes().first() == Some(&b'/') {
            EXT4_ROOT_INODE
        } else {
            start_inode_number
        };
        let mut remaining = trim_leading_separators(path);

        if remaining.is_empty() {
            let inode = self.read_inode(current_inode_number)?;
            return Ok((current_inode_number, inode));
        }

        while !remaining.is_empty() {
            let current_inode = self.read_inode(current_inode_number)?;
            if !current_inode.is_directory() {
                return Err(Ext4Error::NotDirectory);
            }

            let (component, rest) = split_path_component(remaining)?;
            if component == "." {
                remaining = rest;
                continue;
            }
            if component == ".." {
                return Err(Ext4Error::InvalidPath);
            }

            let entry = self.find_directory_entry(&current_inode, component)?;
            let child_inode = self.read_inode(entry.inode_number)?;
            if child_inode.is_symlink() {
                if symlink_depth >= EXT4_MAX_SYMLINK_DEPTH {
                    return Err(Ext4Error::SymlinkLoop);
                }

                let mut expanded_path = [0u8; EXT4_PATH_BYTES];
                let expanded_len = self.read_symlink_path(
                    &child_inode,
                    rest,
                    &mut expanded_path,
                )?;
                let expanded = str::from_utf8(&expanded_path[..expanded_len])
                    .map_err(|_| Ext4Error::InvalidPath)?;
                return self.resolve_path_from(
                    current_inode_number,
                    expanded,
                    symlink_depth + 1,
                );
            }

            current_inode_number = entry.inode_number;
            remaining = rest;
        }

        let inode = self.read_inode(current_inode_number)?;
        Ok((current_inode_number, inode))
    }

    /// Reads one symlink target and appends any unresolved tail path.
    ///
    /// # Parameters
    ///
    /// - `inode`: Symlink inode whose target should be decoded.
    /// - `tail`: Remaining unresolved path components after the symlink.
    /// - `buffer`: Scratch buffer that receives the combined path.
    fn read_symlink_path(
        &mut self,
        inode: &Ext4Inode,
        tail: &str,
        buffer: &mut [u8; EXT4_PATH_BYTES],
    ) -> Result<usize, Ext4Error> {
        let target_len = self.read_symlink_target(inode, buffer)?;
        let mut total_len = target_len;
        let tail = trim_leading_separators(tail);
        if tail.is_empty() {
            return Ok(total_len);
        }

        if total_len != 0 && buffer[total_len - 1] != b'/' {
            if total_len >= buffer.len() {
                return Err(Ext4Error::NameTooLong);
            }

            buffer[total_len] = b'/';
            total_len += 1;
        }

        if total_len + tail.len() > buffer.len() {
            return Err(Ext4Error::NameTooLong);
        }

        buffer[total_len..total_len + tail.len()].copy_from_slice(tail.as_bytes());
        Ok(total_len + tail.len())
    }

    /// Reads one symlink target into `buffer` and returns the number of bytes.
    ///
    /// # Parameters
    ///
    /// - `inode`: Symlink inode whose target should be decoded.
    /// - `buffer`: Scratch buffer that receives the symlink target.
    fn read_symlink_target(
        &mut self,
        inode: &Ext4Inode,
        buffer: &mut [u8; EXT4_PATH_BYTES],
    ) -> Result<usize, Ext4Error> {
        let size = usize::try_from(inode.size_bytes)
            .map_err(|_| Ext4Error::NameTooLong)?;
        if size > buffer.len() {
            return Err(Ext4Error::NameTooLong);
        }

        if !inode.uses_extents() && size <= inode.block_data.len() {
            buffer[..size].copy_from_slice(&inode.block_data[..size]);
            return Ok(size);
        }

        self.read_inode_contents(inode, &mut buffer[..size])
    }

    /// Searches one directory inode for one path component.
    ///
    /// # Parameters
    ///
    /// - `directory_inode`: Directory inode to search.
    /// - `component`: Path component name to resolve.
    fn find_directory_entry(
        &mut self,
        directory_inode: &Ext4Inode,
        component: &str,
    ) -> Result<Ext4DirectoryEntry, Ext4Error> {
        let mut extents = [Ext4Extent {
            logical_block: 0,
            block_count: 0,
            physical_block: 0,
        }; EXT4_MAX_EXTENTS];
        let extent_count = self.collect_inode_extents(directory_inode, &mut extents)?;
        let mut block_buffer = [0u8; EXT4_MAX_BLOCK_SIZE];
        let block_size = self.block_size;

        let mut extent_index = 0usize;
        while extent_index < extent_count {
            let extent = extents[extent_index];
            let mut offset_in_extent = 0u16;
            while offset_in_extent < extent.block_count {
                let block = self.read_block(
                    extent.physical_block + u64::from(offset_in_extent),
                    &mut block_buffer,
                )?;

                let mut entry_offset = 0usize;
                while entry_offset + 8 <= block_size {
                    let inode_number = read_u32(block, entry_offset)
                        .ok_or(Ext4Error::InvalidDirectoryEntry)?;
                    let record_length = read_u16(block, entry_offset + 4)
                        .ok_or(Ext4Error::InvalidDirectoryEntry)? as usize;
                    let name_length = *block.get(entry_offset + 6)
                        .ok_or(Ext4Error::InvalidDirectoryEntry)? as usize;

                    if record_length < 8
                        || !record_length.is_multiple_of(4)
                        || entry_offset + record_length > block_size
                        || name_length > record_length - 8
                    {
                        return Err(Ext4Error::InvalidDirectoryEntry);
                    }

                    if inode_number != 0 {
                        let name = &block[entry_offset + 8..entry_offset + 8 + name_length];
                        if name == component.as_bytes() {
                            return Ok(Ext4DirectoryEntry { inode_number });
                        }
                    }

                    entry_offset += record_length;
                }

                offset_in_extent += 1;
            }

            extent_index += 1;
        }

        Err(Ext4Error::NotFound)
    }

    /// Recursively walks all files contained below one ext4 directory inode.
    ///
    /// # Parameters
    ///
    /// - `directory_inode`: Directory inode whose children should be visited.
    /// - `path`: Mutable UTF-8 path buffer reused across recursion.
    /// - `path_len`: Number of bytes currently stored in `path`.
    /// - `visitor`: Callback invoked once per discovered file-like entry.
    fn walk_directory<F>(
        &mut self,
        directory_inode: &Ext4Inode,
        path: &mut [u8; EXT4_PATH_BYTES],
        path_len: usize,
        visitor: &mut F,
    ) -> Result<(), Ext4Error>
    where
        F: FnMut(&str, u64),
    {
        let mut extents = [Ext4Extent {
            logical_block: 0,
            block_count: 0,
            physical_block: 0,
        }; EXT4_MAX_EXTENTS];
        let extent_count = self.collect_inode_extents(directory_inode, &mut extents)?;
        let mut block_buffer = [0u8; EXT4_MAX_BLOCK_SIZE];
        let block_size = self.block_size;

        let mut extent_index = 0usize;
        while extent_index < extent_count {
            let extent = extents[extent_index];
            let mut offset_in_extent = 0u16;
            while offset_in_extent < extent.block_count {
                let block = self.read_block(
                    extent.physical_block + u64::from(offset_in_extent),
                    &mut block_buffer,
                )?;

                let mut entry_offset = 0usize;
                while entry_offset + 8 <= block_size {
                    let inode_number = read_u32(block, entry_offset)
                        .ok_or(Ext4Error::InvalidDirectoryEntry)?;
                    let record_length = read_u16(block, entry_offset + 4)
                        .ok_or(Ext4Error::InvalidDirectoryEntry)? as usize;
                    let name_length = *block.get(entry_offset + 6)
                        .ok_or(Ext4Error::InvalidDirectoryEntry)? as usize;

                    if record_length < 8
                        || !record_length.is_multiple_of(4)
                        || entry_offset + record_length > block_size
                        || name_length > record_length - 8
                    {
                        return Err(Ext4Error::InvalidDirectoryEntry);
                    }

                    if inode_number != 0 {
                        let name = &block[entry_offset + 8..entry_offset + 8 + name_length];
                        if name != b"." && name != b".." {
                            let next_path_len = append_path_component(path, path_len, name)?;
                            let child_inode = self.read_inode(inode_number)?;
                            if child_inode.is_directory() {
                                self.walk_directory(
                                    &child_inode,
                                    path,
                                    next_path_len,
                                    visitor,
                                )?;
                            } else {
                                let file_path = str::from_utf8(&path[..next_path_len])
                                    .map_err(|_| Ext4Error::InvalidDirectoryEntry)?;
                                visitor(file_path, child_inode.size_bytes);
                            }
                        }
                    }

                    entry_offset += record_length;
                }

                offset_in_extent += 1;
            }

            extent_index += 1;
        }

        Ok(())
    }

    /// Reads one regular-file inode into `buffer` and returns the number of bytes.
    ///
    /// # Parameters
    ///
    /// - `inode`: Regular-file or extent-backed symlink inode to read.
    /// - `buffer`: Output buffer that receives the file contents.
    fn read_inode_contents(
        &mut self,
        inode: &Ext4Inode,
        buffer: &mut [u8],
    ) -> Result<usize, Ext4Error> {
        if inode.is_directory() {
            return Err(Ext4Error::IsDirectory);
        }

        let size_bytes = usize::try_from(inode.size_bytes)
            .map_err(|_| Ext4Error::BufferTooSmall)?;
        if buffer.len() < size_bytes {
            return Err(Ext4Error::BufferTooSmall);
        }

        if size_bytes == 0 {
            return Ok(0);
        }

        unsafe {
            ptr::write_bytes(buffer.as_mut_ptr(), 0, size_bytes);
        }

        let mut extents = [Ext4Extent {
            logical_block: 0,
            block_count: 0,
            physical_block: 0,
        }; EXT4_MAX_EXTENTS];
        let extent_count = self.collect_inode_extents(inode, &mut extents)?;
        let mut tail_block = [0u8; EXT4_MAX_BLOCK_SIZE];
        let block_size = self.block_size;
        let mut extent_index = 0usize;
        while extent_index < extent_count {
            let extent = extents[extent_index];
            let file_offset = extent.logical_block as usize * block_size;
            if file_offset >= size_bytes {
                break;
            }

            let extent_capacity = extent.block_count as usize * block_size;
            let extent_bytes = min(size_bytes - file_offset, extent_capacity);
            let full_blocks = extent_bytes / block_size;
            if full_blocks != 0 {
                let bytes = full_blocks * block_size;
                self.device.read_blocks(
                    self.block_to_lba(extent.physical_block),
                    &mut buffer[file_offset..file_offset + bytes],
                )?;
            }

            let tail_bytes = extent_bytes % block_size;
            if tail_bytes != 0 {
                let block = self.read_block(
                    extent.physical_block + full_blocks as u64,
                    &mut tail_block,
                )?;
                buffer[file_offset + full_blocks * block_size..file_offset + extent_bytes]
                    .copy_from_slice(&block[..tail_bytes]);
            }

            extent_index += 1;
        }

        Ok(size_bytes)
    }

    /// Collects each extent stored by `inode` into `extents` and returns the count.
    ///
    /// # Parameters
    ///
    /// - `inode`: Inode whose extent tree should be traversed.
    /// - `extents`: Output array that receives each resolved leaf extent.
    fn collect_inode_extents(
        &mut self,
        inode: &Ext4Inode,
        extents: &mut [Ext4Extent],
    ) -> Result<usize, Ext4Error> {
        if !inode.uses_extents() {
            return Err(Ext4Error::UnsupportedMapping);
        }

        let mut count = 0usize;
        self.collect_extent_node(&inode.block_data, extents, &mut count)?;
        Ok(count)
    }

    /// Collects one extent node into `extents` and updates `count`.
    ///
    /// # Parameters
    ///
    /// - `node`: Extent node bytes beginning with one extent header.
    /// - `extents`: Output array that receives each resolved leaf extent.
    /// - `count`: Running number of collected extents.
    fn collect_extent_node(
        &mut self,
        node: &[u8],
        extents: &mut [Ext4Extent],
        count: &mut usize,
    ) -> Result<(), Ext4Error> {
        let magic = read_u16(node, 0).ok_or(Ext4Error::InvalidExtentTree)?;
        if magic != EXT4_EXTENT_MAGIC {
            return Err(Ext4Error::InvalidExtentTree);
        }

        let entry_count = read_u16(node, 2).ok_or(Ext4Error::InvalidExtentTree)? as usize;
        let depth = read_u16(node, 6).ok_or(Ext4Error::InvalidExtentTree)?;
        if depth > EXT4_MAX_EXTENT_DEPTH {
            return Err(Ext4Error::InvalidExtentTree);
        }
        if 12 + entry_count * 12 > node.len() {
            return Err(Ext4Error::InvalidExtentTree);
        }

        if depth == 0 {
            let mut index = 0usize;
            while index < entry_count {
                let offset = 12 + index * 12;
                let logical_block = read_u32(node, offset).ok_or(Ext4Error::InvalidExtentTree)?;
                let raw_length = read_u16(node, offset + 4).ok_or(Ext4Error::InvalidExtentTree)?;
                if (raw_length & 0x8000) != 0 {
                    return Err(Ext4Error::InvalidExtentTree);
                }

                let physical_high = read_u16(node, offset + 6).ok_or(Ext4Error::InvalidExtentTree)? as u64;
                let physical_low = read_u32(node, offset + 8).ok_or(Ext4Error::InvalidExtentTree)? as u64;
                if *count >= extents.len() {
                    return Err(Ext4Error::InvalidExtentTree);
                }

                extents[*count] = Ext4Extent {
                    logical_block,
                    block_count: raw_length,
                    physical_block: (physical_high << 32) | physical_low,
                };
                *count += 1;

                index += 1;
            }

            return Ok(());
        }

        let mut child_block = [0u8; EXT4_MAX_BLOCK_SIZE];
        let mut index = 0usize;
        while index < entry_count {
            let offset = 12 + index * 12;
            let child_low = read_u32(node, offset + 4).ok_or(Ext4Error::InvalidExtentTree)? as u64;
            let child_high = read_u16(node, offset + 8).ok_or(Ext4Error::InvalidExtentTree)? as u64;
            let child_physical_block = (child_high << 32) | child_low;
            let child = self.read_block(child_physical_block, &mut child_block)?;
            self.collect_extent_node(child, extents, count)?;

            index += 1;
        }

        Ok(())
    }

    /// Reads one inode record by inode number.
    ///
    /// # Parameters
    ///
    /// - `inode_number`: On-disk inode number to decode.
    fn read_inode(&mut self, inode_number: u32) -> Result<Ext4Inode, Ext4Error> {
        if inode_number == 0 {
            return Err(Ext4Error::InvalidInode(inode_number));
        }

        let group = (inode_number - 1) / self.inodes_per_group;
        let index = (inode_number - 1) % self.inodes_per_group;
        let inode_table_block = self.inode_table_block(group)?;
        let inode_offset = inode_table_block
            .checked_mul(self.block_size as u64)
            .and_then(|value| value.checked_add(index as u64 * u64::from(self.inode_size)))
            .ok_or(Ext4Error::InvalidInode(inode_number))?;

        let mut inode_bytes = [0u8; EXT4_MAX_INODE_SIZE];
        self.read_volume_bytes(
            inode_offset,
            &mut inode_bytes[..self.inode_size as usize],
        )?;

        let mode = read_u16(&inode_bytes, 0).ok_or(Ext4Error::InvalidInode(inode_number))?;
        let size_low = read_u32(&inode_bytes, 4).ok_or(Ext4Error::InvalidInode(inode_number))? as u64;
        let flags = read_u32(&inode_bytes, 0x20).ok_or(Ext4Error::InvalidInode(inode_number))?;
        let size_high = read_u32(&inode_bytes, 0x6c).unwrap_or(0) as u64;
        let mut block_data = [0u8; 60];
        block_data.copy_from_slice(&inode_bytes[0x28..0x28 + 60]);

        Ok(Ext4Inode {
            inode_number,
            mode,
            size_bytes: size_low | (size_high << 32),
            flags,
            block_data,
        })
    }

    /// Returns the block number of one block group's inode table.
    ///
    /// # Parameters
    ///
    /// - `group`: Zero-based block-group index.
    fn inode_table_block(&mut self, group: u32) -> Result<u64, Ext4Error> {
        let descriptor_offset = self.group_descriptor_table_block
            .checked_mul(self.block_size as u64)
            .and_then(|value| value.checked_add(group as u64 * u64::from(self.group_descriptor_size)))
            .ok_or(Ext4Error::InvalidSuperblock)?;
        let mut descriptor = [0u8; 64];
        self.read_volume_bytes(
            descriptor_offset,
            &mut descriptor[..self.group_descriptor_size as usize],
        )?;

        let table_low = read_u32(&descriptor, 8).ok_or(Ext4Error::InvalidSuperblock)? as u64;
        let table_high = if self.group_descriptor_size >= 44 {
            read_u32(&descriptor, 0x28).ok_or(Ext4Error::InvalidSuperblock)? as u64
        } else {
            0
        };
        Ok((table_high << 32) | table_low)
    }

    /// Reads one filesystem block into `buffer` and returns the populated prefix.
    ///
    /// # Parameters
    ///
    /// - `block_number`: Filesystem block number to read.
    /// - `buffer`: Scratch block buffer.
    fn read_block<'buffer>(
        &mut self,
        block_number: u64,
        buffer: &'buffer mut [u8; EXT4_MAX_BLOCK_SIZE],
    ) -> Result<&'buffer [u8], Ext4Error> {
        let block_len = self.block_size;
        self.device.read_blocks(
            self.block_to_lba(block_number),
            &mut buffer[..block_len],
        )?;
        Ok(&buffer[..block_len])
    }

    /// Reads one byte range relative to the filesystem start.
    ///
    /// # Parameters
    ///
    /// - `byte_offset`: Filesystem-relative byte offset.
    /// - `buffer`: Destination buffer that receives the requested bytes.
    fn read_volume_bytes(
        &mut self,
        byte_offset: u64,
        buffer: &mut [u8],
    ) -> Result<(), Ext4Error> {
        read_device_bytes(
            self.device,
            self.partition_start_lba,
            byte_offset,
            buffer,
        )
    }

    /// Converts one filesystem block number into the underlying device LBA.
    ///
    /// # Parameters
    ///
    /// - `block_number`: Filesystem block number to translate.
    fn block_to_lba(&self, block_number: u64) -> u64 {
        self.partition_start_lba + block_number * self.sectors_per_block
    }

    /// Reads one file from the mounted volume into `buffer`.
    ///
    /// # Parameters
    ///
    /// - `path`: Absolute or relative path to the file inside the filesystem.
    /// - `buffer`: Output buffer that receives the complete file contents.
    pub fn read_file(
        &mut self,
        path: &str,
        buffer: &mut [u8],
    ) -> Result<usize, Ext4Error> {
        let (_, inode) = self.resolve_path(path, 0)?;
        self.read_inode_contents(&inode, buffer)
    }
}

/// Reads one unaligned byte range from `device` relative to `partition_start_lba`.
///
/// # Parameters
///
/// - `device`: Underlying block device that backs the filesystem.
/// - `partition_start_lba`: First LBA of the filesystem partition.
/// - `byte_offset`: Filesystem-relative byte offset to read.
/// - `buffer`: Destination buffer that receives the requested bytes.
fn read_device_bytes<D: BlockDevice>(
    device: &mut D,
    partition_start_lba: u64,
    byte_offset: u64,
    buffer: &mut [u8],
) -> Result<(), Ext4Error> {
    if buffer.is_empty() {
        return Ok(());
    }

    if byte_offset.is_multiple_of(VIRTIO_SECTOR_SIZE as u64)
        && buffer.len().is_multiple_of(VIRTIO_SECTOR_SIZE)
    {
        device.read_blocks(
            partition_start_lba + byte_offset / VIRTIO_SECTOR_SIZE as u64,
            buffer,
        )?;
        return Ok(());
    }

    let mut sector = [0u8; VIRTIO_SECTOR_SIZE];
    let mut copied = 0usize;
    while copied < buffer.len() {
        let absolute_offset = byte_offset + copied as u64;
        let sector_index = absolute_offset / VIRTIO_SECTOR_SIZE as u64;
        let sector_offset = (absolute_offset % VIRTIO_SECTOR_SIZE as u64) as usize;
        device.read_blocks(partition_start_lba + sector_index, &mut sector)?;
        let take = min(buffer.len() - copied, VIRTIO_SECTOR_SIZE - sector_offset);
        buffer[copied..copied + take]
            .copy_from_slice(&sector[sector_offset..sector_offset + take]);
        copied += take;
    }

    Ok(())
}

/// Splits `path` into the next component and the remaining suffix.
///
/// # Parameters
///
/// - `path`: Remaining unresolved path string.
fn split_path_component(path: &str) -> Result<(&str, &str), Ext4Error> {
    let path = trim_leading_separators(path);
    if path.is_empty() {
        return Err(Ext4Error::InvalidPath);
    }

    match path.find('/') {
        Some(index) => Ok((&path[..index], trim_leading_separators(&path[index + 1..]))),
        None => Ok((path, "")),
    }
}

/// Trims leading `/` separators from `path`.
///
/// # Parameters
///
/// - `path`: Path string whose leading separators should be skipped.
fn trim_leading_separators(path: &str) -> &str {
    let mut start = 0usize;
    let bytes = path.as_bytes();
    while start < bytes.len() && bytes[start] == b'/' {
        start += 1;
    }
    &path[start..]
}

/// Appends one directory-entry name to a reusable absolute path buffer.
///
/// # Parameters
///
/// - `path`: UTF-8 path buffer updated in place.
/// - `path_len`: Number of initialized bytes already stored in `path`.
/// - `component`: Directory-entry name to append.
fn append_path_component(
    path: &mut [u8; EXT4_PATH_BYTES],
    path_len: usize,
    component: &[u8],
) -> Result<usize, Ext4Error> {
    let next_len = path_len
        .checked_add(1)
        .and_then(|len| len.checked_add(component.len()))
        .ok_or(Ext4Error::NameTooLong)?;
    if next_len > path.len() {
        return Err(Ext4Error::NameTooLong);
    }

    let separator_index = path_len;
    path[separator_index] = b'/';
    let component_start = separator_index + 1;
    path[component_start..next_len].copy_from_slice(component);
    Ok(next_len)
}

/// Converts one file size in bytes into the corresponding number of 4 KiB pages.
///
/// # Parameters
///
/// - `size_bytes`: File size in bytes.
fn file_size_to_page_count(size_bytes: usize) -> Result<usize, Ext4Error> {
    let page_size = EFI_PAGE_SIZE as usize;
    size_bytes
        .checked_add(page_size - 1)
        .map(|value| value / page_size)
        .ok_or(Ext4Error::BufferTooSmall)
}

/// Reads one little-endian `u16` from `bytes`.
///
/// # Parameters
///
/// - `bytes`: Byte slice containing the encoded value.
/// - `offset`: Starting byte offset of the value.
fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let data = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([data[0], data[1]]))
}

/// Reads one little-endian `u32` from `bytes`.
///
/// # Parameters
///
/// - `bytes`: Byte slice containing the encoded value.
/// - `offset`: Starting byte offset of the value.
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let data = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}