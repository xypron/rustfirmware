//! Device-tree memory-region interpretation helpers.
//!
//! This module builds memory-oriented queries on top of the generic flattened
//! device-tree reader. It decodes RAM ranges from `/memory`, merges reserved
//! regions from both the reserve map and `/reserved-memory`, and reserves the
//! original FDT blob in the EFI-style allocator.

use crate::dtb_read::{read_cells, Fdt};
use crate::memory::{MemoryError, PageAllocator};

/// One memory or reserved-memory range decoded from an FDT.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryRegion {
    /// Start address of the decoded region.
    pub base: u64,
    /// Size in bytes of the decoded region.
    pub size: u64,
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
    allocator.reserve_region(MemoryRegion {
        base: fdt.base_ptr() as u64,
        size: u64::try_from(fdt.total_size_bytes())
            .map_err(|_| MemoryError::AddressOverflow)?,
    })
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

        if let Some(device_type) = fdt.get_property_string(node, "device_type") {
            if device_type != "memory" {
                return true;
            }
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
fn reserve_map_regions(fdt: &Fdt<'_>, output: &mut [MemoryRegion]) -> usize {
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
    let stride = (address_cells + size_cells) * 4;
    if stride == 0 {
        return 0;
    }

    let mut count = 0usize;
    let mut index = 0usize;
    while index + stride <= reg.len() && count < output.len() {
        let base = read_cells(&reg[index..index + address_cells * 4], address_cells);
        let size = read_cells(&reg[index + address_cells * 4..index + stride], size_cells);
        output[count] = MemoryRegion { base, size };
        count += 1;
        index += stride;
    }

    count
}