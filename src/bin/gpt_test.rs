#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

//! Host-side GPT fixture validation binary.
//!
//! This tool loads minimal GPT disk images from `tests/data`, applies the
//! shared parser in `src/gpt.rs`, and verifies the expected parse outcome for
//! each fixture.

#[cfg(target_os = "none")]
use core::panic::PanicInfo;

/// Minimal host-side block-device shim used by the shared GPT parser.
#[cfg(not(target_os = "none"))]
mod virtio {
    /// Sector size used by the GPT parser.
    pub const VIRTIO_SECTOR_SIZE: usize = 512;

    /// Minimal error surface needed by the shared `BlockDevice` interface.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum VirtioError {
        /// The caller supplied a buffer whose length is not a whole number of sectors.
        InvalidBufferLength,
        /// One host file operation failed while reading sectors.
        IoFailure,
    }

    /// Minimal block-device abstraction used by the GPT parser.
    pub trait BlockDevice {
        /// Returns the total number of 512-byte sectors exposed by the device.
        fn sector_count(&self) -> u64;

        /// Reads one or more contiguous 512-byte sectors into `buffer`.
        fn read_blocks(&mut self, sector: u64, buffer: &mut [u8]) -> Result<(), VirtioError>;
    }
}

#[cfg(not(target_os = "none"))]
#[path = "../partition.rs"]
mod partition;

#[cfg(not(target_os = "none"))]
#[allow(dead_code)]
#[path = "../gpt.rs"]
mod gpt;

/// Host-side GPT fixture validation implementation.
#[cfg(not(target_os = "none"))]
mod host {
use std::env;
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::gpt::GptPartitionTable;
use crate::partition::{PartitionEntry, PartitionTable};
use crate::virtio::{BlockDevice, VIRTIO_SECTOR_SIZE, VirtioError};

/// Directory containing minimal GPT image fixtures.
const DEFAULT_FIXTURE_DIR: &str = "tests/data";
/// Filename prefix for fixtures that must parse successfully.
const SUCCESS_PREFIX: &str = "ok_";
/// Filename prefix for fixtures that must be rejected.
const FAILURE_PREFIX: &str = "fail_";
/// Expected label stored in the valid fixture partition.
const EXPECTED_LABEL: &str = "rootfs";

/// Expected parse result for one image fixture.
enum FixtureExpectation {
    /// The fixture must parse and expose one expected Linux partition.
    ParseSuccess,
    /// The fixture must be rejected by the GPT parser.
    ParseFailure,
    /// The fixture has no encoded expectation and is only summarized.
    NoExpectation,
}

/// Invocation modes supported by the host-side GPT test binary.
enum TestMode {
    /// Discover and validate every `.img` fixture under `tests/data`.
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

/// Runs the host-side GPT fixture validation flow.
pub fn run() -> Result<(), Box<dyn Error>> {
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
        _ => Err(io::Error::other("usage: cargo run --target <host> --bin gpt_test [image.img]").into()),
    }
}

/// Discovers and validates every GPT image fixture.
fn run_all_fixtures() -> Result<(), Box<dyn Error>> {
    let mut fixtures = collect_fixture_images(Path::new(DEFAULT_FIXTURE_DIR))?;
    fixtures.sort();

    if fixtures.is_empty() {
        return Err(io::Error::other("no .img GPT fixtures found").into());
    }

    for fixture in fixtures {
        run_one_case(&fixture)?;
    }

    Ok(())
}

/// Collects all `.img` fixtures from one directory.
fn collect_fixture_images(directory: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut fixtures = Vec::new();

    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let is_img = path.extension().and_then(|value| value.to_str()) == Some("img");
        if is_img {
            fixtures.push(path);
        }
    }

    Ok(fixtures)
}

/// Applies the GPT parser to one fixture and validates the expected outcome.
fn run_one_case(path: &Path) -> Result<(), Box<dyn Error>> {
    let expectation = expectation_for(path);
    let mut device = FileBlockDevice::open(path)?;
    let table = GptPartitionTable::new(&mut device);

    match expectation {
        FixtureExpectation::ParseSuccess => {
            let mut table = table.ok_or_else(|| {
                io::Error::other(format!("expected {} to parse successfully", path.display()))
            })?;
            verify_expected_table(&mut table, path)?;
            println!("gpt_test: {} parsed successfully", path.display());
        }
        FixtureExpectation::ParseFailure => {
            if table.is_some() {
                return Err(io::Error::other(format!(
                    "expected {} to be rejected",
                    path.display(),
                ))
                .into());
            }
            println!("gpt_test: {} rejected as expected", path.display());
        }
        FixtureExpectation::NoExpectation => {
            if let Some(mut table) = table {
                let present = summarize_present_partitions(&mut table)?;
                println!(
                    "gpt_test: {} parsed with {} present partition(s)",
                    path.display(),
                    present,
                );
            } else {
                println!("gpt_test: {} rejected", path.display());
            }
        }
    }

    Ok(())
}

/// Returns the expected parse result encoded in a fixture filename.
fn expectation_for(path: &Path) -> FixtureExpectation {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return FixtureExpectation::NoExpectation;
    };

    if name.starts_with(SUCCESS_PREFIX) {
        FixtureExpectation::ParseSuccess
    } else if name.starts_with(FAILURE_PREFIX) {
        FixtureExpectation::ParseFailure
    } else {
        FixtureExpectation::NoExpectation
    }
}

/// Verifies that one parsed fixture exposes the expected single Linux partition.
fn verify_expected_table<D: BlockDevice>(
    table: &mut GptPartitionTable<'_, D>,
    path: &Path,
) -> Result<(), Box<dyn Error>> {
    if table.partition_count() != 1 {
        return Err(io::Error::other(format!(
            "{}: expected exactly one GPT slot, found {}",
            path.display(),
            table.partition_count(),
        ))
        .into());
    }

    let entry = table.partition(0).ok_or_else(|| {
        io::Error::other(format!("{}: failed to read partition entry 0", path.display()))
    })?;
    if !entry.is_present() {
        return Err(io::Error::other(format!(
            "{}: expected entry 0 to be present",
            path.display(),
        ))
        .into());
    }

    verify_expected_partition(&entry, path)
}

/// Summarizes how many populated partitions were parsed from one table.
fn summarize_present_partitions<D: BlockDevice>(
    table: &mut GptPartitionTable<'_, D>,
) -> Result<u32, Box<dyn Error>> {
    let mut present = 0u32;

    for index in 0..table.partition_count() {
        let entry = table.partition(index).ok_or_else(|| {
            io::Error::other(format!("failed to read partition entry {index}"))
        })?;
        if entry.is_present() {
            present += 1;
        }
    }

    Ok(present)
}

/// Verifies the single expected partition payload shared by the valid fixtures.
fn verify_expected_partition<E: PartitionEntry>(
    entry: &E,
    path: &Path,
) -> Result<(), Box<dyn Error>> {
    if entry.first_lba() != 3 || entry.last_lba() != 3 || entry.sector_count() != 1 {
        return Err(io::Error::other(format!(
            "{}: unexpected LBA range {}-{}",
            path.display(),
            entry.first_lba(),
            entry.last_lba(),
        ))
        .into());
    }

    if entry.bootable() || entry.is_efi_system_partition() {
        return Err(io::Error::other(format!(
            "{}: partition flags do not match the expected Linux data partition",
            path.display(),
        ))
        .into());
    }

    let mut label_buffer = [0u8; 72];
    if entry.label(&mut label_buffer) != EXPECTED_LABEL {
        return Err(io::Error::other(format!(
            "{}: unexpected partition label {}",
            path.display(),
            entry.label(&mut label_buffer),
        ))
        .into());
    }

    let mut type_buffer = [0u8; 36];
    if entry.partition_type(&mut type_buffer) != "Linux filesystem" {
        return Err(io::Error::other(format!(
            "{}: unexpected partition type {}",
            path.display(),
            entry.partition_type(&mut type_buffer),
        ))
        .into());
    }

    Ok(())
}
}

/// Runs the host-side GPT fixture validation binary on hosted targets.
#[cfg(not(target_os = "none"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    host::run()
}

/// Handles unrecoverable failures for the freestanding stub build.
#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// Provides a no-op entry point so this host-only binary still links for the
/// firmware target.
#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    loop {
        core::hint::spin_loop();
    }
}