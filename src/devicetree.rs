//! Flattened device tree parsing helpers.
//!
//! The current implementation is intentionally read-only. It validates the FDT
//! header, exposes RAM ranges from `/memory`, and reports reserved regions from
//! both the reserve map and the `/reserved-memory` subtree. The module is meant
//! to grow into a future device-tree editing layer.

use core::slice;
use core::str;

/// Flattened device tree header magic value.
const FDT_MAGIC: u32 = 0xd00d_feed;
/// Structure token marking the start of a node.
const FDT_BEGIN_NODE: u32 = 1;
/// Structure token marking the end of a node.
const FDT_END_NODE: u32 = 2;
/// Structure token marking a property record.
const FDT_PROP: u32 = 3;
/// Structure token marking a no-op padding entry.
const FDT_NOP: u32 = 4;
/// Structure token marking the end of the structure block.
const FDT_END: u32 = 9;

/// One memory or reserved-memory range decoded from an FDT.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryRegion {
    /// Start address of the decoded region.
    pub base: u64,
    /// Size in bytes of the decoded region.
    pub size: u64,
}

/// Errors returned while validating the flattened device tree blob.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FdtError {
    /// The blob header did not contain the FDT magic value.
    BadMagic,
    /// The blob or one of its sections ended before required data was available.
    Truncated,
    /// The blob version is older than the minimum supported format version.
    UnsupportedVersion,
}

/// Read-only view of one flattened device tree blob.
pub struct Fdt<'a> {
    /// Raw reserve map section of the FDT blob.
    reserve_map: &'a [u8],
    /// Raw structure block section of the FDT blob.
    structure: &'a [u8],
    /// Raw strings block section of the FDT blob.
    strings: &'a [u8],
}

impl<'a> Fdt<'a> {
    /// Creates an FDT view from a raw pointer passed by the boot environment.
    ///
    /// The caller must guarantee that `ptr_raw` points at a valid FDT blob in
    /// readable memory for the duration of the returned view.
    ///
    /// # Parameters
    ///
    /// - `ptr_raw`: Raw pointer to the start of the flattened device tree blob.
    pub unsafe fn from_ptr(ptr_raw: *const u8) -> Result<Self, FdtError> {
        if read_be_u32(ptr_raw, 0).ok_or(FdtError::Truncated)? != FDT_MAGIC {
            return Err(FdtError::BadMagic);
        }

        let total_size = read_be_u32(ptr_raw, 4).ok_or(FdtError::Truncated)? as usize;
        let off_dt_struct = read_be_u32(ptr_raw, 8).ok_or(FdtError::Truncated)? as usize;
        let off_dt_strings = read_be_u32(ptr_raw, 12).ok_or(FdtError::Truncated)? as usize;
        let off_mem_rsvmap = read_be_u32(ptr_raw, 16).ok_or(FdtError::Truncated)? as usize;
        let version = read_be_u32(ptr_raw, 20).ok_or(FdtError::Truncated)?;
        let size_dt_strings = read_be_u32(ptr_raw, 32).ok_or(FdtError::Truncated)? as usize;
        let size_dt_struct = read_be_u32(ptr_raw, 36).ok_or(FdtError::Truncated)? as usize;

        if version < 17 {
            return Err(FdtError::UnsupportedVersion);
        }

        if off_mem_rsvmap > total_size
            || off_dt_struct + size_dt_struct > total_size
            || off_dt_strings + size_dt_strings > total_size
        {
            return Err(FdtError::Truncated);
        }

        let blob = unsafe { slice::from_raw_parts(ptr_raw, total_size) };
        Ok(Self {
            reserve_map: &blob[off_mem_rsvmap..],
            structure: &blob[off_dt_struct..off_dt_struct + size_dt_struct],
            strings: &blob[off_dt_strings..off_dt_strings + size_dt_strings],
        })
    }

    /// Collects memory ranges from `/memory` nodes into `output`.
    ///
    /// # Parameters
    ///
    /// - `output`: Destination slice that receives decoded RAM ranges.
    pub fn memory_regions(&self, output: &mut [MemoryRegion]) -> usize {
        let mut cursor = 0usize;
        let mut depth = 0usize;
        let mut root_address_cells = 2u32;
        let mut root_size_cells = 1u32;
        let mut current_address_cells = [2u32; 16];
        let mut current_size_cells = [1u32; 16];
        let mut memory_depth: Option<usize> = None;
        let mut count = 0usize;

        while let Some(token) = self.read_token(cursor) {
            cursor += 4;

            match token {
                FDT_BEGIN_NODE => {
                    let (name, next_cursor) = match self.read_c_string(self.structure, cursor) {
                        Some(value) => value,
                        None => break,
                    };
                    cursor = align4(next_cursor);

                    if depth + 1 < current_address_cells.len() {
                        current_address_cells[depth + 1] = current_address_cells[depth];
                        current_size_cells[depth + 1] = current_size_cells[depth];
                    }

                    if (name == "memory" || name.starts_with("memory@")) && depth == 1 {
                        memory_depth = Some(depth + 1);
                    }

                    depth += 1;
                }
                FDT_END_NODE => {
                    if memory_depth == Some(depth) {
                        memory_depth = None;
                    }

                    if depth == 0 {
                        break;
                    }

                    depth -= 1;
                }
                FDT_PROP => {
                    let len = match self.read_token(cursor) {
                        Some(value) => value as usize,
                        None => break,
                    };
                    let nameoff = match self.read_token(cursor + 4) {
                        Some(value) => value as usize,
                        None => break,
                    };
                    let value_offset = cursor + 8;
                    let value_end = value_offset + len;
                    if value_end > self.structure.len() {
                        break;
                    }

                    let name = match self.string_at(nameoff) {
                        Some(value) => value,
                        None => break,
                    };
                    let value = &self.structure[value_offset..value_end];

                    if depth == 1 && name == "#address-cells" {
                        if let Some(cells) = read_be_u32_from_slice(value, 0) {
                            root_address_cells = cells;
                            current_address_cells[depth] = cells;
                        }
                    } else if depth == 1 && name == "#size-cells" {
                        if let Some(cells) = read_be_u32_from_slice(value, 0) {
                            root_size_cells = cells;
                            current_size_cells[depth] = cells;
                        }
                    } else if depth < current_address_cells.len() && name == "#address-cells" {
                        if let Some(cells) = read_be_u32_from_slice(value, 0) {
                            current_address_cells[depth] = cells;
                        }
                    } else if depth < current_size_cells.len() && name == "#size-cells" {
                        if let Some(cells) = read_be_u32_from_slice(value, 0) {
                            current_size_cells[depth] = cells;
                        }
                    } else if memory_depth == Some(depth) && name == "device_type" {
                        if value != b"memory\0" {
                            memory_depth = None;
                        }
                    } else if memory_depth == Some(depth) && name == "reg" {
                        let address_cells = root_address_cells as usize;
                        let size_cells = root_size_cells as usize;
                        let stride = (address_cells + size_cells) * 4;

                        if stride != 0 {
                            let mut index = 0usize;
                            while index + stride <= value.len() && count < output.len() {
                                let base = read_cells(&value[index..index + address_cells * 4], address_cells);
                                let size = read_cells(
                                    &value[index + address_cells * 4..index + stride],
                                    size_cells,
                                );
                                output[count] = MemoryRegion { base, size };
                                count += 1;
                                index += stride;
                            }
                        }
                    }

                    cursor = align4(value_end);
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break,
            }
        }

        count
    }

    /// Collects reserved regions from both the FDT reserve map and the
    /// `/reserved-memory` subtree into `output`.
    ///
    /// # Parameters
    ///
    /// - `output`: Destination slice that receives decoded reserved ranges.
    pub fn reserved_regions(&self, output: &mut [MemoryRegion]) -> usize {
        let mut count = self.reserve_map_regions(output);
        if count < output.len() {
            count += self.reserved_memory_regions(&mut output[count..]);
        }
        count
    }

    fn reserve_map_regions(&self, output: &mut [MemoryRegion]) -> usize {
        let mut offset = 0usize;
        let mut count = 0usize;

        while count < output.len() {
            let address = match read_be_u64_from_slice(self.reserve_map, offset) {
                Some(value) => value,
                None => break,
            };
            let size = match read_be_u64_from_slice(self.reserve_map, offset + 8) {
                Some(value) => value,
                None => break,
            };

            if address == 0 && size == 0 {
                break;
            }

            output[count] = MemoryRegion { base: address, size };
            count += 1;
            offset += 16;
        }

        count
    }

    fn reserved_memory_regions(&self, output: &mut [MemoryRegion]) -> usize {
        let mut cursor = 0usize;
        let mut depth = 0usize;
        let mut current_address_cells = [2u32; 16];
        let mut current_size_cells = [1u32; 16];
        let mut reserved_depth: Option<usize> = None;
        let mut count = 0usize;

        while let Some(token) = self.read_token(cursor) {
            cursor += 4;

            match token {
                FDT_BEGIN_NODE => {
                    let (name, next_cursor) = match self.read_c_string(self.structure, cursor) {
                        Some(value) => value,
                        None => break,
                    };
                    cursor = align4(next_cursor);

                    if depth + 1 < current_address_cells.len() {
                        current_address_cells[depth + 1] = current_address_cells[depth];
                        current_size_cells[depth + 1] = current_size_cells[depth];
                    }

                    if name == "reserved-memory" && depth == 1 {
                        reserved_depth = Some(depth + 1);
                    }

                    depth += 1;
                }
                FDT_END_NODE => {
                    if reserved_depth == Some(depth) {
                        reserved_depth = None;
                    }

                    if depth == 0 {
                        break;
                    }

                    depth -= 1;
                }
                FDT_PROP => {
                    let len = match self.read_token(cursor) {
                        Some(value) => value as usize,
                        None => break,
                    };
                    let nameoff = match self.read_token(cursor + 4) {
                        Some(value) => value as usize,
                        None => break,
                    };
                    let value_offset = cursor + 8;
                    let value_end = value_offset + len;
                    if value_end > self.structure.len() {
                        break;
                    }

                    let name = match self.string_at(nameoff) {
                        Some(value) => value,
                        None => break,
                    };
                    let value = &self.structure[value_offset..value_end];

                    if depth == 1 && name == "#address-cells" {
                        if let Some(cells) = read_be_u32_from_slice(value, 0) {
                            current_address_cells[depth] = cells;
                        }
                    } else if depth == 1 && name == "#size-cells" {
                        if let Some(cells) = read_be_u32_from_slice(value, 0) {
                            current_size_cells[depth] = cells;
                        }
                    } else if depth < current_address_cells.len() && name == "#address-cells" {
                        if let Some(cells) = read_be_u32_from_slice(value, 0) {
                            current_address_cells[depth] = cells;
                        }
                    } else if depth < current_size_cells.len() && name == "#size-cells" {
                        if let Some(cells) = read_be_u32_from_slice(value, 0) {
                            current_size_cells[depth] = cells;
                        }
                    } else if reserved_depth.is_some() && depth > reserved_depth.unwrap_or(0) && name == "reg" {
                        let address_cells = current_address_cells[reserved_depth.unwrap_or(depth)] as usize;
                        let size_cells = current_size_cells[reserved_depth.unwrap_or(depth)] as usize;
                        let stride = (address_cells + size_cells) * 4;

                        if stride != 0 {
                            let mut index = 0usize;
                            while index + stride <= value.len() && count < output.len() {
                                let base = read_cells(&value[index..index + address_cells * 4], address_cells);
                                let size = read_cells(
                                    &value[index + address_cells * 4..index + stride],
                                    size_cells,
                                );
                                output[count] = MemoryRegion { base, size };
                                count += 1;
                                index += stride;
                            }
                        }
                    }

                    cursor = align4(value_end);
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break,
            }
        }

        count
    }

    fn read_token(&self, offset: usize) -> Option<u32> {
        read_be_u32_from_slice(self.structure, offset)
    }

    fn string_at(&self, offset: usize) -> Option<&'a str> {
        let bytes = self.strings.get(offset..)?;
        let end = bytes.iter().position(|byte| *byte == 0)?;
        str::from_utf8(&bytes[..end]).ok()
    }

    fn read_c_string<'b>(&self, bytes: &'b [u8], offset: usize) -> Option<(&'b str, usize)> {
        let rest = bytes.get(offset..)?;
        let end = rest.iter().position(|byte| *byte == 0)?;
        let name = str::from_utf8(&rest[..end]).ok()?;
        Some((name, offset + end + 1))
    }
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

fn read_be_u32(ptr_raw: *const u8, offset: usize) -> Option<u32> {
    let word_ptr = unsafe { ptr_raw.add(offset) };
    let bytes = unsafe { slice::from_raw_parts(word_ptr, 4) };
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_be_u32_from_slice(bytes: &[u8], offset: usize) -> Option<u32> {
    let word = bytes.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([word[0], word[1], word[2], word[3]]))
}

fn read_be_u64_from_slice(bytes: &[u8], offset: usize) -> Option<u64> {
    let word = bytes.get(offset..offset + 8)?;
    Some(u64::from_be_bytes([
        word[0], word[1], word[2], word[3], word[4], word[5], word[6], word[7],
    ]))
}

fn read_cells(bytes: &[u8], cells: usize) -> u64 {
    let mut value = 0u64;
    let mut index = 0usize;

    while index < cells {
        let offset = index * 4;
        let cell = read_be_u32_from_slice(bytes, offset).unwrap_or(0) as u64;
        value = (value << 32) | cell;
        index += 1;
    }

    value
}