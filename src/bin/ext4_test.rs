#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

//! Host-side ext4 validation binary.
//!
//! This tool opens one GPT disk image, mounts the first Linux filesystem
//! partition through `src/ext4.rs`, and verifies that `/boot/vmlinuz` can be
//! resolved, loaded, and read end to end.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::env;
use std::error::Error;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {
        std::println!($($arg)*)
    };
}

mod memory {
    //! Minimal std-backed page-allocation shim used by the host-side ext4 test.

    use super::{Layout, PhantomData, alloc_zeroed, dealloc};

    /// EFI page size used by the production filesystem load path.
    pub const EFI_PAGE_SIZE: u64 = 4096;
    /// Minimal EFI physical address type used by the shared filesystem code.
    pub type EFI_PHYSICAL_ADDRESS = u64;

    /// EFI memory types needed by the shared ext4 code under test.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    #[repr(u32)]
    pub enum EFI_MEMORY_TYPE {
        /// Boot-services data used for loaded file contents.
        EfiBootServicesData = 4,
    }

    /// EFI page-allocation policies needed by the shared ext4 code under test.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    #[repr(u32)]
    pub enum EFI_ALLOCATE_TYPE {
        /// Allocate pages at any suitable host address.
        AllocateAnyPages = 0,
        /// Allocate pages at one specific host address.
        AllocateAddress = 2,
    }

    /// Allocation failures reported by the host-side page-allocation shim.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum MemoryError {
        /// The caller supplied invalid allocation parameters.
        InvalidParameter,
        /// The host allocator could not satisfy the request.
        OutOfResources,
    }

    /// Host-backed page allocator used to exercise shared file-loading code.
    pub struct PageAllocator<'a> {
        /// Host allocations that must be freed when the allocator is dropped.
        allocations: Vec<(*mut u8, Layout)>,
        /// Lifetime marker matching the production allocator signature.
        marker: PhantomData<&'a mut ()>,
    }

    impl<'a> PageAllocator<'a> {
        /// Creates an empty host-side page allocator.
        pub fn new() -> Self {
            Self {
                allocations: Vec::new(),
                marker: PhantomData,
            }
        }

        /// Allocates zeroed pages using EFI-style parameter names.
        pub fn AllocatePages(
            &mut self,
            Type: EFI_ALLOCATE_TYPE,
            _MemoryType: EFI_MEMORY_TYPE,
            Pages: usize,
            Memory: &mut EFI_PHYSICAL_ADDRESS,
        ) -> Result<(), MemoryError> {
            if Pages == 0 {
                return Err(MemoryError::InvalidParameter);
            }

            let size = Pages
                .checked_mul(EFI_PAGE_SIZE as usize)
                .ok_or(MemoryError::OutOfResources)?;
            let layout = Layout::from_size_align(size, EFI_PAGE_SIZE as usize)
                .map_err(|_| MemoryError::InvalidParameter)?;
            let pointer = unsafe { alloc_zeroed(layout) };
            if pointer.is_null() {
                return Err(MemoryError::OutOfResources);
            }

            self.allocations.push((pointer, layout));
            let _ = Type;
            *Memory = pointer as EFI_PHYSICAL_ADDRESS;
            Ok(())
        }

        /// Allocates enough pages to cover `size_bytes`.
        pub fn allocate_pages_for_size(
            &mut self,
            memory_type: EFI_MEMORY_TYPE,
            size_bytes: usize,
        ) -> Result<EFI_PHYSICAL_ADDRESS, MemoryError> {
            let size = size_bytes.max(1);
            let pages = size.div_ceil(EFI_PAGE_SIZE as usize);
            let mut memory = 0;
            self.AllocatePages(
                EFI_ALLOCATE_TYPE::AllocateAnyPages,
                memory_type,
                pages,
                &mut memory,
            )?;
            Ok(memory)
        }
    }

    impl Drop for PageAllocator<'_> {
        /// Frees all outstanding host allocations owned by the shim.
        fn drop(&mut self) {
            for (pointer, layout) in self.allocations.drain(..) {
                unsafe {
                    dealloc(pointer, layout);
                }
            }
        }
    }
}

mod virtio {
    //! Minimal host-side block-device shim used by the shared parsers.

    /// Sector size used by the GPT and ext4 parsers.
    pub const VIRTIO_SECTOR_SIZE: usize = 512;

    /// Minimal error surface needed by the shared `BlockDevice` interface.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum VirtioError {
        /// The caller supplied a buffer whose length is not a whole number of sectors.
        InvalidBufferLength,
        /// One host file operation failed while reading sectors.
        IoFailure,
    }

    /// Minimal block-device abstraction used by the shared filesystem parsers.
    pub trait BlockDevice {
        /// Returns the total number of 512-byte sectors exposed by the device.
        fn sector_count(&self) -> u64;

        /// Reads one or more contiguous 512-byte sectors into `buffer`.
        fn read_blocks(&mut self, sector: u64, buffer: &mut [u8]) -> Result<(), VirtioError>;
    }
}

mod fat {
    //! Minimal stub so shared filesystem helpers can compile in the ext4 host test.

    use crate::virtio::BlockDevice;

    /// Host-test FAT stub used only to satisfy shared filesystem imports.
    pub struct FatVolume;

    impl FatVolume {
        /// The ext4 host test never mounts FAT, so this always reports failure.
        pub fn new<D: BlockDevice>(
            _device: &mut D,
            _partition_start_lba: u64,
        ) -> Result<Self, ()> {
            Err(())
        }
    }
}

#[allow(dead_code)]
#[path = "../filesystem.rs"]
mod filesystem;
#[allow(dead_code)]
#[path = "../partition.rs"]
mod partition;
#[allow(dead_code)]
#[path = "../gpt.rs"]
mod gpt;
#[allow(dead_code)]
#[path = "../ext4.rs"]
mod ext4;

use ext4::Ext4Volume;
use filesystem::{FileHandle, FileInfoView, FileSystem, FileType};
use gpt::GptPartitionTable;
use memory::PageAllocator;
use partition::{PartitionEntry, PartitionTable};
use virtio::{BlockDevice, VIRTIO_SECTOR_SIZE, VirtioError};

/// Default GPT image used by the host-side ext4 test.
const DEFAULT_IMAGE_PATH: &str = "test.img";
/// ext4 path expected to resolve through the Linux root filesystem.
const EXPECTED_PATH: &str = "/boot/vmlinuz";
/// Lower size bound that rejects accidentally following one symlink instead of the real kernel image.
const MIN_EXPECTED_KERNEL_SIZE: usize = 1 << 20;

/// Host-backed block device that reads sectors from one image file.
struct FileBlockDevice {
    /// Open image file handle.
    file: File,
    /// Total file size expressed in 512-byte sectors.
    sector_count: u64,
}

impl FileBlockDevice {
    /// Opens one image file and validates that its size is sector-aligned.
    fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        let file = File::open(path)?;
        let length = file.metadata()?.len();
        if length == 0 || (length % VIRTIO_SECTOR_SIZE as u64) != 0 {
            return Err(io::Error::other(format!(
                "{} is not a non-empty {}-byte sector image",
                path.display(),
                VIRTIO_SECTOR_SIZE,
            ))
            .into());
        }

        Ok(Self {
            file,
            sector_count: length / VIRTIO_SECTOR_SIZE as u64,
        })
    }
}

impl BlockDevice for FileBlockDevice {
    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn read_blocks(&mut self, sector: u64, buffer: &mut [u8]) -> Result<(), VirtioError> {
        if buffer.is_empty() || (buffer.len() % VIRTIO_SECTOR_SIZE) != 0 {
            return Err(VirtioError::InvalidBufferLength);
        }

        let offset = sector
            .checked_mul(VIRTIO_SECTOR_SIZE as u64)
            .ok_or(VirtioError::IoFailure)?;
        let end = offset
            .checked_add(buffer.len() as u64)
            .ok_or(VirtioError::IoFailure)?;
        let image_len = self.sector_count * VIRTIO_SECTOR_SIZE as u64;
        if end > image_len {
            return Err(VirtioError::IoFailure);
        }

        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|_| VirtioError::IoFailure)?;
        self.file
            .read_exact(buffer)
            .map_err(|_| VirtioError::IoFailure)
    }
}

/// Runs the host-side ext4 validation flow.
fn main() -> Result<(), Box<dyn Error>> {
    let image_path = parse_image_path()?;
    let mut device = FileBlockDevice::open(&image_path)?;
    let partition_start = linux_partition_start(&mut device, &image_path)?;
    let mut volume = Ext4Volume::new(&mut device, partition_start)
        .map_err(|error| io::Error::other(format!("{}: failed to mount ext4: {error:?}", image_path.display())))?;

    let mut file = volume.open(EXPECTED_PATH)
        .map_err(|error| io::Error::other(format!("{}: failed to open {}: {error:?}", image_path.display(), EXPECTED_PATH)))?;
    if file.file_type() != FileType::File {
        return Err(io::Error::other(format!(
            "{}: {} did not resolve to a regular file",
            image_path.display(),
            EXPECTED_PATH,
        ))
        .into());
    }

    let info = file.info();
    if info.size_bytes() < MIN_EXPECTED_KERNEL_SIZE {
        return Err(io::Error::other(format!(
            "{}: {} resolved to an unexpectedly small object ({})",
            image_path.display(),
            EXPECTED_PATH,
            info.size_bytes(),
        ))
        .into());
    }

    let mut allocator = PageAllocator::new();
    let loaded = file.load(&mut allocator)
        .map_err(|error| io::Error::other(format!("{}: failed to load {}: {error:?}", image_path.display(), EXPECTED_PATH)))?;
    if loaded.bytes().len() != info.size_bytes() {
        return Err(io::Error::other(format!(
            "{}: loaded byte count {} did not match inode size {}",
            image_path.display(),
            loaded.bytes().len(),
            info.size_bytes(),
        ))
        .into());
    }

    if loaded.bytes().iter().take(64).all(|byte| *byte == 0) {
        return Err(io::Error::other(format!(
            "{}: {} loaded all-zero header bytes",
            image_path.display(),
            EXPECTED_PATH,
        ))
        .into());
    }

    println!(
        "ext4_test: {} mounted successfully and loaded {} ({} bytes)",
        image_path.display(),
        EXPECTED_PATH,
        info.size_bytes(),
    );
    Ok(())
}

/// Parses the optional image-path argument.
fn parse_image_path() -> Result<PathBuf, Box<dyn Error>> {
    let mut args = env::args_os();
    let _program = args.next();
    let first = args.next();
    let second = args.next();

    match (first, second) {
        (None, None) => Ok(PathBuf::from(DEFAULT_IMAGE_PATH)),
        (Some(path), None) => Ok(PathBuf::from(path)),
        _ => Err(io::Error::other("usage: cargo run --target <host> --bin ext4_test [image.img]").into()),
    }
}

/// Returns the first GPT partition whose type string is `Linux filesystem`.
fn linux_partition_start<D: BlockDevice>(
    device: &mut D,
    image_path: &Path,
) -> Result<u64, Box<dyn Error>> {
    let mut partitions = GptPartitionTable::new(device)
        .ok_or_else(|| io::Error::other(format!("{}: no valid GPT found", image_path.display())))?;
    let mut label = [0u8; 72];
    let mut partition_type = [0u8; 36];
    let _ = &mut label;

    for index in 0..partitions.partition_count() {
        let entry = partitions.partition(index).ok_or_else(|| {
            io::Error::other(format!("{}: failed to read GPT entry {index}", image_path.display()))
        })?;
        if entry.is_present() && entry.partition_type(&mut partition_type) == "Linux filesystem" {
            return Ok(entry.first_lba());
        }
    }

    Err(io::Error::other(format!("{}: no Linux filesystem partition found", image_path.display())).into())
}