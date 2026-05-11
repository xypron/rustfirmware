#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

//! Host-side DTB validation binary.
//!
//! This tool discovers `in*.dts` fixtures under `tests/data`, compiles them to
//! DTB form with `dtc`, mutates the resulting blobs through `src/dtb_write.rs`, writes
//! matching `out*.dtb` files, and finally re-runs `dtc` to verify the mutated
//! trees decode cleanly and contain the expected `/chosen` properties.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::env;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::process::Command;

mod memory {
    //! Minimal std-backed page-allocation shim used by the host-side DTB test.

    use super::{Layout, PhantomData, alloc_zeroed, dealloc};

    /// EFI page size used by the production DTB clone path.
    pub const EFI_PAGE_SIZE: u64 = 4096;

    /// EFI memory types needed by the shared DTB code under test.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    #[repr(u32)]
    pub enum EFI_MEMORY_TYPE {
        /// ACPI reclaim memory type used for cloned device trees.
        EfiACPIReclaimMemory = 9,
    }

    /// EFI page-allocation policies needed by the shared DTB code under test.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    #[repr(u32)]
    pub enum EFI_ALLOCATE_TYPE {
        /// Allocate pages at any suitable host address.
        AllocateAnyPages = 0,
    }

    /// Allocation failures reported by the host-side page-allocation shim.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum MemoryError {
        /// The caller supplied invalid allocation parameters.
        InvalidParameter,
        /// The host allocator could not satisfy the request.
        OutOfResources,
    }

    /// Host-backed page allocator used to exercise the shared DTB clone path.
    pub struct PageAllocator<'a> {
        /// Host allocations that must be freed when the allocator is dropped.
        allocations: Vec<(*mut u8, Layout)>,
        /// Lifetime marker matching the production allocator signature.
        marker: PhantomData<&'a mut ()>,
    }

    impl<'a> PageAllocator<'a> {
        /// Creates an empty host-side page allocator.
        ///
        /// # Parameters
        ///
        /// This function does not accept parameters.
        pub fn new() -> Self {
            Self {
                allocations: Vec::new(),
                marker: PhantomData,
            }
        }

        /// Allocates zeroed pages using EFI-style parameter names.
        ///
        /// # Parameters
        ///
        /// - `Type`: Allocation policy to apply.
        /// - `_MemoryType`: Memory type associated with the allocation.
        /// - `Pages`: Number of 4 KiB pages to allocate.
        /// - `Memory`: Output pointer to the allocated host buffer.
        pub fn AllocatePages(
            &mut self,
            Type: EFI_ALLOCATE_TYPE,
            _MemoryType: EFI_MEMORY_TYPE,
            Pages: usize,
            Memory: &mut u64,
        ) -> Result<(), MemoryError> {
            if Type != EFI_ALLOCATE_TYPE::AllocateAnyPages || Pages == 0 {
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
            *Memory = pointer as u64;
            Ok(())
        }
    }

    impl Drop for PageAllocator<'_> {
        /// Frees all outstanding host allocations owned by the shim.
        ///
        /// # Parameters
        ///
        /// This function does not accept parameters.
        fn drop(&mut self) {
            for (pointer, layout) in self.allocations.drain(..) {
                unsafe {
                    dealloc(pointer, layout);
                }
            }
        }
    }
}

#[allow(dead_code)]
#[path = "../dtb_read.rs"]
mod dtb_read;

#[allow(dead_code)]
#[path = "../dtb_write.rs"]
mod dtb_write;

use dtb_write::Dtb;
use dtb_write::DtbError;
use memory::PageAllocator;

/// Directory holding host-side DTB test fixtures.
const DEFAULT_FIXTURE_DIR: &str = "tests/data";
/// Expected Linux command line inserted into the `/chosen` node.
const BOOTARGS: &str = "console=ttyS0 root=/dev/vda";
/// Expected initrd start address inserted into the `/chosen` node.
const INITRD_START: u64 = 0x1122_3344_5566_7788;
/// Expected initrd end address inserted into the `/chosen` node.
const INITRD_END: u64 = 0x8877_6655_4433_2211;
/// Extra DTB capacity reserved before mutating one fixture.
const EXTRA_DTB_BYTES: usize = 8 * 1024;

/// Owns one 8-byte-aligned host buffer used to load a DTB fixture.
struct OwnedAlignedBuffer {
    /// Host pointer returned by the aligned allocator.
    pointer: *mut u8,
    /// Layout used to free the aligned allocation.
    layout: Layout,
}

impl OwnedAlignedBuffer {
    /// Copies `bytes` into one freshly allocated 8-byte-aligned host buffer.
    ///
    /// # Parameters
    ///
    /// - `bytes`: Raw DTB bytes to copy into the aligned buffer.
    fn from_bytes(bytes: &[u8]) -> Result<Self, Box<dyn Error>> {
        let layout = Layout::from_size_align(bytes.len(), 8)?;
        let pointer = unsafe { alloc_zeroed(layout) };
        if pointer.is_null() {
            return Err(Box::new(TestError("failed to allocate aligned input buffer")));
        }

        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
        }

        Ok(Self { pointer, layout })
    }

    /// Returns the immutable pointer to the start of the aligned buffer.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn as_ptr(&self) -> *const u8 {
        self.pointer.cast_const()
    }
}

impl Drop for OwnedAlignedBuffer {
    /// Frees the aligned host buffer when the wrapper is dropped.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn drop(&mut self) {
        unsafe {
            dealloc(self.pointer, self.layout);
        }
    }
}

/// Fixed text error used for simple host-side validation failures.
#[derive(Debug)]
struct TestError(&'static str);

impl Display for TestError {
    /// Formats the stored test error string.
    ///
    /// # Parameters
    ///
    /// - `f`: Formatter receiving the error text.
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl Error for TestError {}

/// Runs the host-side DTB fixture validation flow.
///
/// # Parameters
///
/// This function does not accept parameters.
fn main() -> Result<(), Box<dyn Error>> {
    match parse_mode()? {
        TestMode::AllFixtures => run_all_fixtures(),
        TestMode::SinglePaths { input_dtb, output_dtb } => {
            run_one_case(&input_dtb, &output_dtb, None)
        }
    }
}

/// Invocation modes supported by the host-side DTB test binary.
enum TestMode {
    /// Discover and validate every `in*.dts` fixture under `tests/data`.
    AllFixtures,
    /// Validate one explicit input/output DTB pair supplied on the command line.
    SinglePaths {
        /// Explicit input DTB path.
        input_dtb: PathBuf,
        /// Explicit output DTB path.
        output_dtb: PathBuf,
    },
}

/// Discovers and validates every host-side DTB fixture.
///
/// # Parameters
///
/// This function does not accept parameters.
fn run_all_fixtures() -> Result<(), Box<dyn Error>> {
    let mut fixtures = collect_fixture_dts(Path::new(DEFAULT_FIXTURE_DIR))?;
    fixtures.sort();

    if fixtures.is_empty() {
        return Err(Box::new(TestError("no in*.dts fixtures found")));
    }

    for fixture in fixtures {
        let input_dtb = fixture.with_extension("dtb");
        let output_dtb = output_path_for_fixture(&fixture)?;
        run_one_case(&input_dtb, &output_dtb, Some(&fixture))?;
    }

    Ok(())
}

/// Executes the DTB mutation and verification flow for one fixture.
///
/// # Parameters
///
/// - `input_dtb`: Input DTB path to load.
/// - `output_dtb`: Output DTB path to write.
/// - `input_dts`: Optional DTS fixture to compile before loading `input_dtb`.
fn run_one_case(
    input_dtb: &Path,
    output_dtb: &Path,
    input_dts: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    if let Some(input_dts) = input_dts {
        generate_input_dtb(input_dts, input_dtb)?;
    }

    let input_bytes = fs::read(input_dtb)?;
    let input_buffer = OwnedAlignedBuffer::from_bytes(&input_bytes)?;
    let input_tree = unsafe { Dtb::from_ptr(input_buffer.as_ptr()) }
        .map_err(|error| dtb_error("from_ptr", error))?;

    let mut allocator = PageAllocator::new();
    let mut output_tree = input_tree.clone(
        input_tree.size() + EXTRA_DTB_BYTES,
        &mut allocator,
    ).map_err(|error| dtb_error("clone", error))?;

    output_tree
        .create_node("/chosen")
        .map_err(|error| dtb_error("create_node(/chosen)", error))?;
    output_tree
        .set_property_u64("/chosen", "linux,initrd-start", INITRD_START)
        .map_err(|error| dtb_error("set_property_u64(linux,initrd-start)", error))?;
    output_tree
        .set_property_u64("/chosen", "linux,initrd-end", INITRD_END)
        .map_err(|error| dtb_error("set_property_u64(linux,initrd-end)", error))?;
    output_tree
        .set_property_string("/chosen", "bootargs", BOOTARGS)
        .map_err(|error| dtb_error("set_property_string(bootargs)", error))?;

    if let Some(parent) = output_dtb.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output_dtb, output_tree.bytes())?;

    let dts = run_dtc(output_dtb)?;
    validate_dts(&dts)?;

    println!(
        "dtb_test: wrote {} and validated chosen properties",
        output_dtb.display()
    );
    Ok(())
}

/// Parses the command-line mode for the host-side DTB validation binary.
///
/// # Parameters
///
/// This function does not accept parameters.
fn parse_mode() -> Result<TestMode, Box<dyn Error>> {
    let mut args = env::args_os();
    let _program = args.next();
    let first = args.next();
    let second = args.next();
    let third = args.next();

    match (first, second, third) {
        (None, None, None) => Ok(TestMode::AllFixtures),
        (Some(input), Some(output), None) => Ok(TestMode::SinglePaths {
            input_dtb: PathBuf::from(input),
            output_dtb: PathBuf::from(output),
        }),
        _ => Err(Box::new(TestError(
            "usage: cargo run --bin dtb_test [in.dtb out.dtb]",
        ))),
    }
}

/// Collects every `in*.dts` fixture in `directory`.
///
/// # Parameters
///
/// - `directory`: Fixture directory to scan.
fn collect_fixture_dts(directory: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut fixtures = Vec::new();

    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            continue;
        }

        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !file_name.starts_with("in") || !file_name.ends_with(".dts") {
            continue;
        }

        fixtures.push(path);
    }

    Ok(fixtures)
}

/// Derives the matching `out*.dtb` path for one `in*.dts` fixture.
///
/// # Parameters
///
/// - `input_dts`: Input DTS fixture path.
fn output_path_for_fixture(input_dts: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let stem = input_dts
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or(TestError("fixture file name must be valid UTF-8"))?;
    if !stem.starts_with("in") {
        return Err(Box::new(TestError("fixture file name must start with 'in'")));
    }

    let output_stem = format!("out{}", &stem[2..]);
    Ok(input_dts.with_file_name(output_stem).with_extension("dtb"))
}

/// Compiles one DTS fixture to DTB form with `dtc`.
///
/// # Parameters
///
/// - `input_dts`: DTS fixture path to compile.
/// - `input_dtb`: DTB output path to create.
fn generate_input_dtb(input_dts: &Path, input_dtb: &Path) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = input_dtb.parent() {
        fs::create_dir_all(parent)?;
    }

    let output = Command::new("dtc")
        .args([
            "-I",
            "dts",
            "-O",
            "dtb",
            input_dts.to_str().ok_or(TestError("non-utf8 input dts path"))?,
            "-o",
            input_dtb.to_str().ok_or(TestError("non-utf8 input dtb path"))?,
        ])
        .output()?;

    if !output.status.success() {
        return Err(format!(
            "dtc failed to generate input dtb:\n{}",
            String::from_utf8_lossy(&output.stderr),
        )
        .into());
    }

    if !output.stderr.is_empty() {
        return Err(format!(
            "dtc reported diagnostics while generating input dtb:\n{}",
            String::from_utf8_lossy(&output.stderr),
        )
        .into());
    }

    Ok(())
}

/// Decodes one DTB back to DTS form with `dtc`.
///
/// # Parameters
///
/// - `output_dtb`: DTB path to decode.
fn run_dtc(output_dtb: &Path) -> Result<String, Box<dyn Error>> {
    let output = Command::new("dtc")
        .args([
            "-I",
            "dtb",
            "-O",
            "dts",
            output_dtb.to_str().ok_or(TestError("non-utf8 output dtb path"))?,
        ])
        .output()?;

    if !output.status.success() {
        return Err(format!(
            "dtc failed to decode output dtb:\n{}",
            String::from_utf8_lossy(&output.stderr),
        )
        .into());
    }

    if !output.stderr.is_empty() {
        return Err(format!(
            "dtc reported diagnostics for output dtb:\n{}",
            String::from_utf8_lossy(&output.stderr),
        )
        .into());
    }

    Ok(String::from_utf8(output.stdout)?)
}

/// Checks that the rendered DTS contains the expected `/chosen` properties.
///
/// # Parameters
///
/// - `dts`: DTS text rendered by `dtc` from the mutated DTB.
fn validate_dts(dts: &str) -> Result<(), Box<dyn Error>> {
    require_contains(dts, "chosen {")?;
    require_contains(dts, "linux,initrd-start")?;
    require_contains(dts, "0x11223344")?;
    require_contains(dts, "0x55667788")?;
    require_contains(dts, "linux,initrd-end")?;
    require_contains(dts, "0x88776655")?;
    require_contains(dts, "0x44332211")?;
    require_contains(dts, "bootargs = \"console=ttyS0 root=/dev/vda\";")?;
    Ok(())
}

/// Requires `haystack` to contain `needle`.
///
/// # Parameters
///
/// - `haystack`: Larger string to search.
/// - `needle`: Expected substring.
fn require_contains(haystack: &str, needle: &str) -> Result<(), Box<dyn Error>> {
    if haystack.contains(needle) {
        return Ok(());
    }

    Err(format!("expected DTS output to contain: {needle}").into())
}

/// Wraps one `DtbError` with additional operation context.
///
/// # Parameters
///
/// - `context`: Description of the DTB operation that failed.
/// - `error`: DTB-layer error to wrap.
fn dtb_error(context: &str, error: DtbError) -> Box<dyn Error> {
    format!("dtb mutation failed in {context}: {error:?}").into()
}