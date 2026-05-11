//! Device-tree memory-region interpretation helpers.
//!
//! This module builds memory-oriented queries on top of the generic flattened
//! device-tree reader. It decodes RAM ranges from `/memory`, merges reserved
//! regions from both the reserve map and `/reserved-memory`, and reserves the
//! original FDT blob in the EFI-style allocator.

use crate::dtb_read::{read_cells, Fdt};
use crate::memory::{EFI_MEMORY_TYPE, MemoryError, PageAllocator};

/// One memory or reserved-memory range decoded from an FDT.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryRegion {
    /// Start address of the decoded region.
    pub base: u64,
    /// Size in bytes of the decoded region.
    pub size: u64,
}

/// UEFI-facing classification of one static `/reserved-memory` node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservedMemoryType {
    /// Region has `no-map` and must remain reserved in the UEFI memory map.
    Reserved,
    /// Region is statically reserved but should appear as boot-services data.
    BootServicesData,
}

/// Reserves the original FDT blob in one EFI-style page allocator.
///
/// # Parameters
///
/// - `fdt`: Flattened device tree whose backing storage must be reserved.
/// - `allocator`: Page allocator whose map should reserve the original FDT.
pub fn reserve_original_fdt(
    fdt: &Fdt<'_>,
    allocator: &mut PageAllocator<'_>,
) -> Result<(), MemoryError> {
    allocator.reserve_region_with_type(
        MemoryRegion {
            base: fdt.base_ptr() as u64,
            size: u64::try_from(fdt.total_size_bytes())
                .map_err(|_| MemoryError::AddressOverflow)?,
        },
        EFI_MEMORY_TYPE::EfiACPIReclaimMemory,
    )
}

/// Collects memory ranges from `/memory` nodes into `output`.
///
/// # Parameters
///
/// - `fdt`: Flattened device tree to inspect.
/// - `output`: Destination slice that receives decoded RAM ranges.
pub fn memory_regions(fdt: &Fdt<'_>, output: &mut [MemoryRegion]) -> usize {
    let Some(root) = fdt.root_node() else {
        return 0;
    };

    let root_address_cells = fdt.get_property_u32(root, "#address-cells").unwrap_or(2);
    let root_size_cells = fdt.get_property_u32(root, "#size-cells").unwrap_or(1);
    let mut count = 0usize;

    fdt.for_each_child(root, |node| {
        if count == output.len() {
            return false;
        }

        if node.name != "memory" && !node.name.starts_with("memory@") {
            return true;
        }

        if let Some(device_type) = fdt.get_property_string(node, "device_type")
            && device_type != "memory"
        {
            return true;
        }

        if let Some(reg) = fdt.get_property(node, "reg") {
            count += decode_regions(
                reg,
                root_address_cells as usize,
                root_size_cells as usize,
                &mut output[count..],
            );
        }

        true
    });

    count
}

/// Collects reserved regions from both the FDT reserve map and the
/// `/reserved-memory` subtree into `output`.
///
/// # Parameters
///
/// - `fdt`: Flattened device tree to inspect.
/// - `output`: Destination slice that receives decoded reserved ranges.
pub fn reserved_regions(fdt: &Fdt<'_>, output: &mut [MemoryRegion]) -> usize {
    let Some(root) = fdt.root_node() else {
        return 0;
    };

    let root_address_cells = fdt.get_property_u32(root, "#address-cells").unwrap_or(2);
    let root_size_cells = fdt.get_property_u32(root, "#size-cells").unwrap_or(1);

    let mut count = reserve_map_regions(fdt, output);
    if count < output.len() {
        count += reserved_memory_regions(
            fdt,
            root_address_cells,
            root_size_cells,
            &mut output[count..],
        );
    }
    count
}

/// Collects reserved regions from the FDT reserve map into `output`.
///
/// # Parameters
///
/// - `fdt`: Flattened device tree supplying the reserve map section.
/// - `output`: Destination slice that receives decoded reserved ranges.
pub fn reserve_map_regions(fdt: &Fdt<'_>, output: &mut [MemoryRegion]) -> usize {
    let mut count = 0usize;

    while let Some(entry) = fdt.reserve_entry(count) {
        if count == output.len() {
            break;
        }

        output[count] = MemoryRegion {
            base: entry.address,
            size: entry.size,
        };
        count += 1;
    }

    count
}

/// Visits each static region under `/reserved-memory` together with its UEFI
/// memory-map classification.
///
/// Dynamic reserved-memory nodes are skipped because they are allocated by the
/// operating system after firmware boot services exit.
///
/// # Parameters
///
/// - `fdt`: Flattened device tree to inspect.
/// - `visit`: Callback invoked once per decoded static reserved-memory range.
pub fn for_each_static_reserved_memory_region(
    fdt: &Fdt<'_>,
    mut visit: impl FnMut(MemoryRegion, ReservedMemoryType) -> bool,
) {
    let Some(root) = fdt.root_node() else {
        return;
    };

    let root_address_cells = fdt.get_property_u32(root, "#address-cells").unwrap_or(2);
    let root_size_cells = fdt.get_property_u32(root, "#size-cells").unwrap_or(1);
    let Some(reserved_node) = fdt.find_node("/reserved-memory") else {
        return;
    };

    let address_cells = fdt.get_property_u32(reserved_node, "#address-cells")
        .unwrap_or(root_address_cells) as usize;
    let size_cells = fdt.get_property_u32(reserved_node, "#size-cells")
        .unwrap_or(root_size_cells) as usize;

    fdt.for_each_child(reserved_node, |node| {
        let Some(reg) = fdt.get_property(node, "reg") else {
            return true;
        };

        let memory_type = if fdt.get_property(node, "no-map").is_some() {
            ReservedMemoryType::Reserved
        } else {
            ReservedMemoryType::BootServicesData
        };

        visit_decoded_regions(reg, address_cells, size_cells, |region| {
            visit(region, memory_type)
        })
    });
}

/// Collects reserved regions from the `/reserved-memory` subtree into `output`.
///
/// # Parameters
///
/// - `fdt`: Flattened device tree to inspect.
/// - `output`: Destination slice that receives decoded reserved ranges.
fn reserved_memory_regions(
    fdt: &Fdt<'_>,
    root_address_cells: u32,
    root_size_cells: u32,
    output: &mut [MemoryRegion],
) -> usize {
    let Some(reserved_node) = fdt.find_node("/reserved-memory") else {
        return 0;
    };

    let address_cells = fdt.get_property_u32(reserved_node, "#address-cells")
        .unwrap_or(root_address_cells) as usize;
    let size_cells = fdt.get_property_u32(reserved_node, "#size-cells")
        .unwrap_or(root_size_cells) as usize;
    let mut count = 0usize;

    fdt.for_each_child(reserved_node, |node| {
        if count == output.len() {
            return false;
        }

        if let Some(reg) = fdt.get_property(node, "reg") {
            count += decode_regions(reg, address_cells, size_cells, &mut output[count..]);
        }

        true
    });

    count
}

/// Decodes one `reg` property payload into `output`.
///
/// # Parameters
///
/// - `reg`: Property payload to decode.
/// - `address_cells`: Number of 32-bit address cells per entry.
/// - `size_cells`: Number of 32-bit size cells per entry.
/// - `output`: Destination slice that receives decoded memory regions.
fn decode_regions(
    reg: &[u8],
    address_cells: usize,
    size_cells: usize,
    output: &mut [MemoryRegion],
) -> usize {
    let mut count = 0usize;
    visit_decoded_regions(reg, address_cells, size_cells, |region| {
        if count == output.len() {
            return false;
        }

        output[count] = region;
        count += 1;
        true
    });

    count
}

/// Visits each region encoded in one `reg` property payload.
///
/// # Parameters
///
/// - `reg`: Property payload to decode.
/// - `address_cells`: Number of 32-bit address cells per entry.
/// - `size_cells`: Number of 32-bit size cells per entry.
/// - `visit`: Callback invoked once per decoded region.
fn visit_decoded_regions(
    reg: &[u8],
    address_cells: usize,
    size_cells: usize,
    mut visit: impl FnMut(MemoryRegion) -> bool,
) -> bool {
    let stride = (address_cells + size_cells) * 4;
    if stride == 0 {
        return true;
    }

    let mut index = 0usize;
    while index + stride <= reg.len() {
        let base = read_cells(&reg[index..index + address_cells * 4], address_cells);
        let size = read_cells(&reg[index + address_cells * 4..index + stride], size_cells);
        if !visit(MemoryRegion { base, size }) {
            return false;
        }
        index += stride;
    }

    true
}