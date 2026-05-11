#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

//! Host-side EFI memory-map validation binary.
//!
//! This tool compiles one DTS fixture with reserved-memory nodes to DTB form,
//! builds the EFI-style memory map through `src/memory.rs`, prints the
//! resulting descriptors, and verifies that static `/reserved-memory` nodes
//! are classified according to the UEFI rules for `no-map` and ordinary
//! reserved regions.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::Path;
use std::process::Command;

#[allow(dead_code)]
#[path = "../dtb_read.rs"]
mod dtb_read;

#[allow(dead_code)]
#[path = "../dtb_memory.rs"]
mod dtb_memory;

#[allow(dead_code)]
#[path = "../memory.rs"]
mod memory;

use dtb_memory::MemoryRegion;
use dtb_read::Fdt;
use memory::{
    memory_map_from_fdt, EFI_MEMORY_DESCRIPTOR, EFI_MEMORY_TYPE,
    EMPTY_MEMORY_DESCRIPTOR,
};

/// DTS fixture used to validate reserved-memory mapping rules.
const INPUT_DTS: &str = "tests/data/memory_map_reserved_regions.dts";
/// DTB path generated under the project-local temporary directory.
const OUTPUT_DTB: &str = "tmp/memory_map_reserved_regions.dtb";
/// Conventional memory address expected to remain unreserved.
const CONVENTIONAL_BASE: u64 = 0x8000_0000;
/// Static `/memreserve/` range expected to remain reserved.
const MEMRESERVE_BASE: u64 = 0x8060_0000;
/// Static `no-map` reserved-memory range expected to be reserved.
const NOMAP_BASE: u64 = 0x8080_0000;
/// Static ordinary reserved-memory range expected as boot-services data.
const BOOT_SERVICES_BASE: u64 = 0x80a0_0000;
/// Dynamic reserved-memory node without `reg` that must not appear reserved.
const DYNAMIC_REGION_BASE: u64 = 0x80c0_0000;
/// Shared region size used by the fixture's reserved-memory nodes.
const REGION_SIZE: u64 = 0x0010_0000;

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
            return Err(Box::new(TestError("failed to allocate aligned DTB buffer")));
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

/// Runs the host-side EFI memory-map validation flow.
///
/// # Parameters
///
/// This function does not accept parameters.
fn main() -> Result<(), Box<dyn Error>> {
    let input_dts = Path::new(INPUT_DTS);
    let output_dtb = Path::new(OUTPUT_DTB);
    generate_input_dtb(input_dts, output_dtb)?;

    let input_bytes = fs::read(output_dtb)?;
    let input_buffer = OwnedAlignedBuffer::from_bytes(&input_bytes)?;
    let fdt = unsafe { Fdt::from_ptr(input_buffer.as_ptr()) }
        .map_err(|error| format!("failed to parse dtb fixture: {error:?}"))?;

    let mut memory_regions = [MemoryRegion { base: 0, size: 0 }; 8];
    let mut reserved_regions = [MemoryRegion { base: 0, size: 0 }; 16];
    let mut descriptors = [EMPTY_MEMORY_DESCRIPTOR; 32];

    let descriptor_count = memory_map_from_fdt(
        &fdt,
        &mut memory_regions,
        &mut reserved_regions,
        &mut descriptors,
    )
    .map_err(|error| format!("failed to build EFI memory map: {error:?}"))?;

    print_memory_map(&descriptors[..descriptor_count]);

    expect_descriptor_type(
        &descriptors[..descriptor_count],
        MEMRESERVE_BASE,
        REGION_SIZE,
        EFI_MEMORY_TYPE::EfiReservedMemoryType,
    )?;
    expect_descriptor_type(
        &descriptors[..descriptor_count],
        NOMAP_BASE,
        REGION_SIZE,
        EFI_MEMORY_TYPE::EfiReservedMemoryType,
    )?;
    expect_descriptor_type(
        &descriptors[..descriptor_count],
        BOOT_SERVICES_BASE,
        REGION_SIZE,
        EFI_MEMORY_TYPE::EfiBootServicesData,
    )?;
    expect_descriptor_type(
        &descriptors[..descriptor_count],
        DYNAMIC_REGION_BASE,
        REGION_SIZE,
        EFI_MEMORY_TYPE::EfiConventionalMemory,
    )?;
    expect_descriptor_type(
        &descriptors[..descriptor_count],
        CONVENTIONAL_BASE,
        REGION_SIZE,
        EFI_MEMORY_TYPE::EfiConventionalMemory,
    )?;

    println!("memory_map_test: memory map matches reserved-memory expectations");
    Ok(())
}

/// Compiles one DTS fixture to DTB form with `dtc`.
///
/// # Parameters
///
/// - `input_dts`: DTS fixture path to compile.
/// - `output_dtb`: DTB output path to create.
fn generate_input_dtb(input_dts: &Path, output_dtb: &Path) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = output_dtb.parent() {
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
            output_dtb.to_str().ok_or(TestError("non-utf8 output dtb path"))?,
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

/// Prints the generated EFI memory map in a compact human-readable form.
///
/// # Parameters
///
/// - `descriptors`: EFI memory descriptors to print.
fn print_memory_map(descriptors: &[EFI_MEMORY_DESCRIPTOR]) {
    for (index, descriptor) in descriptors.iter().copied().enumerate() {
        let size_bytes = descriptor.NumberOfPages.saturating_mul(memory::EFI_PAGE_SIZE);
        println!(
            "efi-memory {}: type={}, base={:#018x}, size={:#018x}",
            index + 1,
            efi_memory_type_name(descriptor.Type),
            descriptor.PhysicalStart,
            size_bytes,
        );
    }
}

/// Verifies that one range is covered by a descriptor of `expected_type`.
///
/// # Parameters
///
/// - `descriptors`: EFI memory descriptors to search.
/// - `base`: Expected physical base address of the region.
/// - `size`: Expected size in bytes of the region.
/// - `expected_type`: EFI memory type that must cover the region.
fn expect_descriptor_type(
    descriptors: &[EFI_MEMORY_DESCRIPTOR],
    base: u64,
    size: u64,
    expected_type: EFI_MEMORY_TYPE,
) -> Result<(), Box<dyn Error>> {
    let end = base
        .checked_add(size)
        .ok_or(TestError("expected descriptor range overflow"))?;

    for descriptor in descriptors {
        let descriptor_end = descriptor
            .PhysicalStart
            .checked_add(descriptor.NumberOfPages.saturating_mul(memory::EFI_PAGE_SIZE))
            .ok_or(TestError("descriptor range overflow"))?;
        if descriptor.PhysicalStart <= base && descriptor_end >= end {
            if descriptor.Type == expected_type as u32 {
                return Ok(());
            }

            return Err(format!(
                "region {base:#018x}-{end:#018x} has type {} instead of {}",
                efi_memory_type_name(descriptor.Type),
                efi_memory_type_name(expected_type as u32),
            )
            .into());
        }
    }

    Err(format!(
        "region {base:#018x}-{end:#018x} not found in EFI memory map",
    )
    .into())
}

/// Returns a short diagnostics name for one EFI memory type.
///
/// # Parameters
///
/// - `memory_type`: EFI memory type encoded as a raw descriptor value.
fn efi_memory_type_name(memory_type: u32) -> &'static str {
    match memory_type {
        0 => "EfiReservedMemoryType",
        1 => "EfiLoaderCode",
        2 => "EfiLoaderData",
        3 => "EfiBootServicesCode",
        4 => "EfiBootServicesData",
        5 => "EfiRuntimeServicesCode",
        6 => "EfiRuntimeServicesData",
        7 => "EfiConventionalMemory",
        8 => "EfiUnusableMemory",
        9 => "EfiACPIReclaimMemory",
        10 => "EfiACPIMemoryNVS",
        11 => "EfiMemoryMappedIO",
        12 => "EfiMemoryMappedIOPortSpace",
        13 => "EfiPalCode",
        14 => "EfiPersistentMemory",
        15 => "EfiUnacceptedMemoryType",
        _ => "unknown",
    }
}