//! Firmware diagnostics output helpers.
//!
//! This module prints raw device-tree information first and then prints the
//! EFI-style page map built from that information by the memory subsystem.

use crate::devicetree::{Fdt, MemoryRegion};
use crate::memory::{memory_map_from_fdt, EFI_MEMORY_DESCRIPTOR, EFI_PAGE_SIZE};

/// Prints firmware diagnostics using the live device tree and memory subsystem.
///
/// # Parameters
///
/// - `boot_hart`: Original hart identifier received in register `a0`.
/// - `device_tree`: Original device-tree pointer received in register `a1`.
/// - `entry_stack`: Original stack pointer value observed before switching stacks.
pub fn print_diagnostics(
    boot_hart: usize,
    device_tree: *const u8,
    entry_stack: usize,
) {
    crate::println!(
        "entry: boot_hart={}, device_tree={:#018x}, sp={:#018x}",
        boot_hart,
        device_tree as usize,
        entry_stack,
    );

    let mut regions = [MemoryRegion { base: 0, size: 0 }; 8];
    let mut reserved = [MemoryRegion { base: 0, size: 0 }; 16];
    let mut memory_map = [EMPTY_MEMORY_DESCRIPTOR; 32];

    let fdt = match unsafe { Fdt::from_ptr(device_tree) } {
        Ok(fdt) => fdt,
        Err(_) => {
            crate::println!("diagnostics: memory-map unavailable");
            return;
        }
    };

    print_fdt_information(&fdt, &mut regions, &mut reserved);
    print_memory_map(&fdt, &mut regions, &mut reserved, &mut memory_map);
}

/// Prints memory and reservation ranges decoded directly from the FDT.
///
/// # Parameters
///
/// - `fdt`: Flattened device tree to inspect.
/// - `memory_regions`: Scratch slice that receives `/memory` ranges.
/// - `reserved_regions`: Scratch slice that receives reserved ranges.
pub fn print_fdt_information(
    fdt: &Fdt,
    memory_regions: &mut [MemoryRegion],
    reserved_regions: &mut [MemoryRegion],
) {
    let memory_region_count = fdt.memory_regions(memory_regions);
    let reserved_region_count = fdt.reserved_regions(reserved_regions);

    let mut index = 0usize;
    while index < memory_region_count {
        crate::println!(
            "memory {}: base={:#018x}, size={:#018x}",
            index + 1,
            memory_regions[index].base as usize,
            memory_regions[index].size as usize,
        );
        index += 1;
    }

    index = 0;
    while index < reserved_region_count {
        crate::println!(
            "reserved {}: base={:#018x}, size={:#018x}",
            index + 1,
            reserved_regions[index].base as usize,
            reserved_regions[index].size as usize,
        );
        index += 1;
    }
}

/// Prints the EFI-style memory map produced by the memory subsystem.
///
/// # Parameters
///
/// - `fdt`: Flattened device tree used to build the map.
/// - `memory_regions`: Scratch slice that receives `/memory` ranges.
/// - `reserved_regions`: Scratch slice that receives reserved ranges.
/// - `memory_map`: Descriptor buffer that receives the EFI-style memory map.
pub fn print_memory_map(
    fdt: &Fdt,
    memory_regions: &mut [MemoryRegion],
    reserved_regions: &mut [MemoryRegion],
    memory_map: &mut [EFI_MEMORY_DESCRIPTOR],
) {
    let descriptor_count = match memory_map_from_fdt(
        fdt,
        memory_regions,
        reserved_regions,
        memory_map,
    ) {
        Ok(descriptor_count) => descriptor_count,
        Err(_) => {
            crate::println!("diagnostics: efi-memory-map unavailable");
            return;
        }
    };

    let mut index = 0usize;
    while index < descriptor_count {
        let descriptor = memory_map[index];
        let size_in_bytes = descriptor.NumberOfPages.saturating_mul(EFI_PAGE_SIZE);
        crate::println!(
            "efi-memory {}: type={}, base={:#018x}, size={:#018x}, attr={:#018x}",
            index + 1,
            efi_memory_type_name(descriptor),
            descriptor.PhysicalStart as usize,
            size_in_bytes as usize,
            descriptor.Attribute as usize,
        );
        index += 1;
    }
}

/// Returns a short diagnostics name for one EFI memory type.
///
/// # Parameters
///
/// - `descriptor`: Descriptor whose type should be formatted.
fn efi_memory_type_name(descriptor: EFI_MEMORY_DESCRIPTOR) -> &'static str {
    match descriptor.Type {
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

/// Empty EFI memory descriptor used to initialize diagnostics scratch storage.
const EMPTY_MEMORY_DESCRIPTOR: EFI_MEMORY_DESCRIPTOR = EFI_MEMORY_DESCRIPTOR {
    Type: 0,
    PhysicalStart: 0,
    VirtualStart: 0,
    NumberOfPages: 0,
    Attribute: 0,
};