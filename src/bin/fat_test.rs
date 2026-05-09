#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

//! Host-side FAT fixture validation binary.
//!
//! This tool loads GPT disk images from `tests/data`, finds the first
//! filesystem partition through `src/gpt.rs`, mounts it through `src/fat.rs`,
//! and validates file access through the shared filesystem traits.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::env;
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

mod memory {
    //! Minimal std-backed page-allocation shim used by the host-side FAT test.

    use super::{Layout, PhantomData, alloc_zeroed, dealloc};

    /// EFI page size used by the production filesystem load path.
    pub const EFI_PAGE_SIZE: u64 = 4096;
    /// Minimal EFI physical address type used by the shared filesystem code.
    pub type EFI_PHYSICAL_ADDRESS = u64;

    /// EFI memory types needed by the shared FAT code under test.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    #[repr(u32)]
    pub enum EFI_MEMORY_TYPE {
        /// Loader data used for loaded file contents.
        EfiLoaderData = 4,
    }

    /// EFI page-allocation policies needed by the shared FAT code under test.
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

            match Type {
                EFI_ALLOCATE_TYPE::AllocateAnyPages => {
                    let pointer = unsafe { alloc_zeroed(layout) };
                    if pointer.is_null() {
                        return Err(MemoryError::OutOfResources);
                    }

                    self.allocations.push((pointer, layout));
                    *Memory = pointer as EFI_PHYSICAL_ADDRESS;
                    Ok(())
                }
                EFI_ALLOCATE_TYPE::AllocateAddress => {
                    if *Memory == 0 {
                        return Err(MemoryError::InvalidParameter);
                    }

                    let pointer = unsafe { alloc_zeroed(layout) };
                    if pointer.is_null() {
                        return Err(MemoryError::OutOfResources);
                    }

                    self.allocations.push((pointer, layout));
                    *Memory = pointer as EFI_PHYSICAL_ADDRESS;
                    Ok(())
                }
            }
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
    //! Minimal host-side block-device shim used by the shared GPT and FAT parsers.

    /// Sector size used by the GPT and FAT parsers.
    pub const VIRTIO_SECTOR_SIZE: usize = 512;

    /// Minimal error surface needed by the shared `BlockDevice` interface.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum VirtioError {
        /// The caller supplied a buffer whose length is not a whole number of sectors.
        InvalidBufferLength,
        /// One host file operation failed while reading sectors.
        IoFailure,
    }

    /// Minimal block-device abstraction used by the GPT and FAT parsers.
    pub trait BlockDevice {
        /// Returns the total number of 512-byte sectors exposed by the device.
        fn sector_count(&self) -> u64;

        /// Reads one or more contiguous 512-byte sectors into `buffer`.
        fn read_blocks(&mut self, sector: u64, buffer: &mut [u8]) -> Result<(), VirtioError>;
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
#[path = "../fat.rs"]
mod fat;

use fat::FatVolume;
use filesystem::{FileHandle, FileInfoView, FileSystem, FileType};
use gpt::GptPartitionTable;
use memory::PageAllocator;
use partition::{PartitionEntry, PartitionTable};
use virtio::{BlockDevice, VIRTIO_SECTOR_SIZE, VirtioError};

/// Directory containing FAT image fixtures.
const DEFAULT_FIXTURE_DIR: &str = "tests/data";
/// Filename prefix for FAT fixtures that must parse successfully.
const FIXTURE_PREFIX: &str = "fat_";
/// File expected inside the FAT fixture.
const EXPECTED_PATH: &str = "/HELLO.TXT";
/// Content expected in the FAT fixture file.
const EXPECTED_CONTENTS: &[u8] = b"hello from fat fixture\n";

/// Invocation modes supported by the host-side FAT test binary.
enum TestMode {
    /// Discover and validate every matching FAT image fixture.
    AllFixtures,
    /// Validate one explicit image path supplied on the command line.
    SinglePath(PathBuf),
}

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

/// Runs the host-side FAT fixture validation flow.
fn main() -> Result<(), Box<dyn Error>> {
    match parse_mode()? {
        TestMode::AllFixtures => run_all_fixtures(),
        TestMode::SinglePath(path) => run_one_case(&path),
    }
}

/// Parses the supported command-line forms.
fn parse_mode() -> Result<TestMode, Box<dyn Error>> {
    let mut args = env::args_os();
    let _program = args.next();
    let first = args.next();
    let second = args.next();

    match (first, second) {
        (None, None) => Ok(TestMode::AllFixtures),
        (Some(path), None) => Ok(TestMode::SinglePath(PathBuf::from(path))),
        _ => Err(io::Error::other("usage: cargo run --target <host> --bin fat_test [image.img]").into()),
    }
}

/// Discovers and validates every FAT image fixture.
fn run_all_fixtures() -> Result<(), Box<dyn Error>> {
    let mut fixtures = collect_fixture_images(Path::new(DEFAULT_FIXTURE_DIR))?;
    fixtures.sort();

    if fixtures.is_empty() {
        return Err(io::Error::other("no FAT .img fixtures found").into());
    }

    for fixture in fixtures {
        run_one_case(&fixture)?;
    }

    Ok(())
}

/// Collects all FAT `.img` fixtures from one directory.
fn collect_fixture_images(directory: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut fixtures = Vec::new();

    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };

        let is_match = name.starts_with(FIXTURE_PREFIX)
            && path.extension().and_then(|value| value.to_str()) == Some("img");
        if is_match {
            fixtures.push(path);
        }
    }

    Ok(fixtures)
}

/// Applies GPT partition discovery and FAT mounting to one fixture.
fn run_one_case(path: &Path) -> Result<(), Box<dyn Error>> {
    let mut device = FileBlockDevice::open(path)?;
    let partition_start_lba = first_partition_lba(&mut device, path)?;
    let mut volume = FatVolume::new(&mut device, partition_start_lba)
        .map_err(|error| io::Error::other(format!("{}: failed to mount FAT volume: {error:?}", path.display())))?;

    let mut file = volume.open(EXPECTED_PATH)
        .map_err(|error| io::Error::other(format!("{}: failed to open {}: {error:?}", path.display(), EXPECTED_PATH)))?;
    if file.file_type() != FileType::File {
        return Err(io::Error::other(format!(
            "{}: {} did not resolve to a regular file",
            path.display(),
            EXPECTED_PATH,
        ))
        .into());
    }

    let info = file.info();
    if info.size_bytes() != EXPECTED_CONTENTS.len() {
        return Err(io::Error::other(format!(
            "{}: {} has size {}, expected {}",
            path.display(),
            EXPECTED_PATH,
            info.size_bytes(),
            EXPECTED_CONTENTS.len(),
        ))
        .into());
    }

    let mut allocator = PageAllocator::new();
    let loaded = file.load(&mut allocator)
        .map_err(|error| io::Error::other(format!("{}: failed to load {}: {error:?}", path.display(), EXPECTED_PATH)))?;
    if loaded.bytes() != EXPECTED_CONTENTS {
        return Err(io::Error::other(format!(
            "{}: {} contents did not match expected test payload",
            path.display(),
            EXPECTED_PATH,
        ))
        .into());
    }

    println!(
        "fat_test: {} mounted successfully and loaded {}",
        path.display(),
        EXPECTED_PATH,
    );
    Ok(())
}

/// Returns the first populated GPT partition start LBA from one fixture.
fn first_partition_lba<D: BlockDevice>(
    device: &mut D,
    path: &Path,
) -> Result<u64, Box<dyn Error>> {
    let mut table = GptPartitionTable::new(device)
        .ok_or_else(|| io::Error::other(format!("{}: no valid GPT found", path.display())))?;

    for index in 0..table.partition_count() {
        let entry = table.partition(index).ok_or_else(|| {
            io::Error::other(format!("{}: failed to read GPT entry {index}", path.display()))
        })?;
        if entry.is_present() {
            return Ok(entry.first_lba());
        }
    }

    Err(io::Error::other(format!("{}: GPT contained no populated partitions", path.display())).into())
}