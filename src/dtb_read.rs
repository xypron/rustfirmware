//! Flattened device tree parsing helpers.
//!
//! This module validates one flattened device-tree blob and exposes read-only
//! node, property, and reserve-map accessors without embedding policy-specific
//! interpretation such as RAM or reserved-memory queries.

use core::mem::size_of;
use core::slice;
use core::str;

/// Size in bytes of the flattened device tree header.
const FDT_HEADER_SIZE: usize = size_of::<u32>() * 10;

/// Flattened device tree header magic value.
pub(crate) const FDT_MAGIC: u32 = 0xd00d_feed;
/// Structure token marking the start of a node.
pub(crate) const FDT_BEGIN_NODE: u32 = 1;
/// Structure token marking the end of a node.
pub(crate) const FDT_END_NODE: u32 = 2;
/// Structure token marking a property record.
pub(crate) const FDT_PROP: u32 = 3;
/// Structure token marking a no-op padding entry.
pub(crate) const FDT_NOP: u32 = 4;
/// Structure token marking the end of the structure block.
pub(crate) const FDT_END: u32 = 9;

/// Errors returned while validating the flattened device tree blob.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FdtError {
    /// The supplied device-tree pointer was null.
    NullPointer,
    /// The supplied device-tree pointer was not 8-byte aligned.
    MisalignedPointer,
    /// The blob header did not contain the FDT magic value.
    BadMagic,
    /// The blob or one of its sections ended before required data was available.
    Truncated,
    /// The blob version is older than the minimum supported format version.
    UnsupportedVersion,
    /// The blob header contained inconsistent offsets or section sizes.
    InvalidHeader,
}

/// One validated node inside the FDT structure block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FdtNode<'a> {
    /// Node name taken directly from the structure block.
    pub name: &'a str,
    /// Absolute byte offset of the node's `FDT_BEGIN_NODE` token.
    begin_offset: usize,
    /// Absolute byte offset of the node's matching `FDT_END_NODE` token.
    end_offset: usize,
}

/// One decoded reserve-map entry from the FDT header area.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FdtReserveEntry {
    /// Start address of the reserved range.
    pub address: u64,
    /// Size in bytes of the reserved range.
    pub size: u64,
}

/// Read-only view of one flattened device tree blob.
pub struct Fdt<'a> {
    /// Raw pointer to the start of the validated FDT blob.
    base: *const u8,
    /// Total size in bytes of the validated FDT blob.
    total_size: usize,
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
        validate_dtb_pointer(ptr_raw)?;

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

        if total_size < FDT_HEADER_SIZE {
            return Err(FdtError::InvalidHeader);
        }

        let struct_end = off_dt_struct
            .checked_add(size_dt_struct)
            .ok_or(FdtError::InvalidHeader)?;
        let strings_end = off_dt_strings
            .checked_add(size_dt_strings)
            .ok_or(FdtError::InvalidHeader)?;

        if off_mem_rsvmap > total_size {
            return Err(FdtError::Truncated);
        }

        if off_dt_struct < FDT_HEADER_SIZE
            || struct_end > total_size
            || off_dt_strings < FDT_HEADER_SIZE
            || strings_end > total_size
        {
            return Err(FdtError::InvalidHeader);
        }

        let blob = unsafe { slice::from_raw_parts(ptr_raw, total_size) };
        Ok(Self {
            base: ptr_raw,
            total_size,
            reserve_map: &blob[off_mem_rsvmap..],
            structure: &blob[off_dt_struct..struct_end],
            strings: &blob[off_dt_strings..strings_end],
        })
    }

    /// Returns the root node from the validated structure block.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn root_node(&self) -> Option<FdtNode<'a>> {
        self.node_at_offset(0)
    }

    /// Finds one direct child node of `parent` by name.
    ///
    /// # Parameters
    ///
    /// - `parent`: Parent node whose direct children should be searched.
    /// - `name`: Name of the direct child node to find.
    pub fn find_child(&self, parent: FdtNode<'a>, name: &str) -> Option<FdtNode<'a>> {
        let mut result = None;

        self.for_each_child(parent, |child| {
            if child.name == name {
                result = Some(child);
                return false;
            }

            true
        });

        result
    }

    /// Finds one node by absolute device-tree path.
    ///
    /// # Parameters
    ///
    /// - `path`: Absolute device-tree path such as `/chosen`.
    pub fn find_node(&self, path: &str) -> Option<FdtNode<'a>> {
        if path == "/" {
            return self.root_node();
        }

        let mut components = path.split('/');
        if components.next()? != "" {
            return None;
        }

        let mut node = self.root_node()?;
        for component in components {
            if component.is_empty() {
                return None;
            }

            node = self.find_child(node, component)?;
        }

        Some(node)
    }

    /// Visits each direct child of `parent` in structure-block order.
    ///
    /// Returning `false` from `visit` stops the iteration early.
    ///
    /// # Parameters
    ///
    /// - `parent`: Parent node whose direct children should be visited.
    /// - `visit`: Callback invoked once for each direct child node.
    pub fn for_each_child(
        &self,
        parent: FdtNode<'a>,
        mut visit: impl FnMut(FdtNode<'a>) -> bool,
    ) {
        let Some(mut offset) = self.after_begin_node(parent.begin_offset) else {
            return;
        };

        while offset < parent.end_offset {
            let Some(token) = self.read_token(offset) else {
                break;
            };

            match token {
                FDT_PROP => {
                    let Some(next_offset) = self.after_property(offset) else {
                        break;
                    };
                    offset = next_offset;
                }
                FDT_NOP => {
                    offset += 4;
                }
                FDT_BEGIN_NODE => {
                    let Some(child) = self.node_at_offset(offset) else {
                        break;
                    };
                    if !visit(child) {
                        break;
                    }
                    let Some(next_offset) = child.end_offset.checked_add(4) else {
                        break;
                    };
                    offset = next_offset;
                }
                FDT_END_NODE | FDT_END => break,
                _ => break,
            }
        }
    }

    /// Returns one property value by name from `node`.
    ///
    /// # Parameters
    ///
    /// - `node`: Node whose direct properties should be searched.
    /// - `property_name`: Name of the property to look up.
    pub fn get_property(
        &self,
        node: FdtNode<'a>,
        property_name: &str,
    ) -> Option<&'a [u8]> {
        let mut offset = self.after_begin_node(node.begin_offset)?;

        while offset < node.end_offset {
            match self.read_token(offset)? {
                FDT_PROP => {
                    let value_length = self.read_token(offset + 4)? as usize;
                    let name_offset = self.read_token(offset + 8)? as usize;
                    let value_offset = offset + 12;
                    let value_end = value_offset.checked_add(value_length)?;
                    let name = self.string_at(name_offset)?;

                    if name == property_name {
                        return self.structure.get(value_offset..value_end);
                    }

                    offset = align4(value_end);
                }
                FDT_NOP => {
                    offset += 4;
                }
                FDT_BEGIN_NODE | FDT_END_NODE | FDT_END => return None,
                _ => return None,
            }
        }

        None
    }

    /// Returns one 32-bit property value decoded from big-endian cells.
    ///
    /// # Parameters
    ///
    /// - `node`: Node that owns the property.
    /// - `property_name`: Name of the property to decode.
    pub fn get_property_u32(
        &self,
        node: FdtNode<'a>,
        property_name: &str,
    ) -> Option<u32> {
        read_be_u32_from_slice(self.get_property(node, property_name)?, 0)
    }

    /// Returns one 64-bit property value decoded from big-endian cells.
    ///
    /// # Parameters
    ///
    /// - `node`: Node that owns the property.
    /// - `property_name`: Name of the property to decode.
    pub fn get_property_u64(
        &self,
        node: FdtNode<'a>,
        property_name: &str,
    ) -> Option<u64> {
        read_be_u64_from_slice(self.get_property(node, property_name)?, 0)
    }

    /// Returns one NUL-terminated UTF-8 property string.
    ///
    /// # Parameters
    ///
    /// - `node`: Node that owns the property.
    /// - `property_name`: Name of the property to decode.
    pub fn get_property_string(
        &self,
        node: FdtNode<'a>,
        property_name: &str,
    ) -> Option<&'a str> {
        let property = self.get_property(node, property_name)?;
        let end = property.iter().position(|byte| *byte == 0)?;
        str::from_utf8(&property[..end]).ok()
    }

    /// Returns the raw start pointer of the validated FDT blob.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub(crate) fn base_ptr(&self) -> *const u8 {
        self.base
    }

    /// Returns the validated total FDT blob size in bytes.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub(crate) fn total_size_bytes(&self) -> usize {
        self.total_size
    }

    /// Returns one decoded reserve-map entry by index.
    ///
    /// # Parameters
    ///
    /// - `index`: Zero-based index of the reserve-map entry to decode.
    pub(crate) fn reserve_entry(&self, index: usize) -> Option<FdtReserveEntry> {
        let offset = index.checked_mul(16)?;
        let address = read_be_u64_from_slice(self.reserve_map, offset)?;
        let size = read_be_u64_from_slice(self.reserve_map, offset + 8)?;
        if address == 0 && size == 0 {
            return None;
        }

        Some(FdtReserveEntry { address, size })
    }

    /// Builds one validated node view from a structure-block offset.
    ///
    /// # Parameters
    ///
    /// - `offset`: Absolute byte offset of one `FDT_BEGIN_NODE` token.
    fn node_at_offset(&self, offset: usize) -> Option<FdtNode<'a>> {
        if self.read_token(offset)? != FDT_BEGIN_NODE {
            return None;
        }

        let name_offset = offset.checked_add(4)?;
        let (name, _) = self.read_c_string(self.structure, name_offset)?;
        let end_offset = self.node_end_offset(offset)?;

        Some(FdtNode {
            name,
            begin_offset: offset,
            end_offset,
        })
    }

    /// Returns the offset immediately after one `FDT_BEGIN_NODE` record.
    ///
    /// # Parameters
    ///
    /// - `begin_offset`: Absolute byte offset of one `FDT_BEGIN_NODE` token.
    fn after_begin_node(&self, begin_offset: usize) -> Option<usize> {
        if self.read_token(begin_offset)? != FDT_BEGIN_NODE {
            return None;
        }

        let name_offset = begin_offset.checked_add(4)?;
        let (_, next_offset) = self.read_c_string(self.structure, name_offset)?;
        Some(align4(next_offset))
    }

    /// Returns the offset immediately after one property record.
    ///
    /// # Parameters
    ///
    /// - `property_offset`: Absolute byte offset of one `FDT_PROP` token.
    fn after_property(&self, property_offset: usize) -> Option<usize> {
        let length = self.read_token(property_offset + 4)? as usize;
        let value_offset = property_offset.checked_add(12)?;
        let value_end = value_offset.checked_add(length)?;
        self.structure.get(value_offset..value_end)?;
        Some(align4(value_end))
    }

    /// Finds the matching `FDT_END_NODE` token for one node.
    ///
    /// # Parameters
    ///
    /// - `begin_offset`: Absolute byte offset of one `FDT_BEGIN_NODE` token.
    fn node_end_offset(&self, begin_offset: usize) -> Option<usize> {
        let mut offset = begin_offset;
        let mut depth = 0usize;

        loop {
            match self.read_token(offset)? {
                FDT_BEGIN_NODE => {
                    depth = depth.checked_add(1)?;
                    offset = self.after_begin_node(offset)?;
                }
                FDT_END_NODE => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(offset);
                    }
                    offset = offset.checked_add(4)?;
                }
                FDT_PROP => {
                    offset = self.after_property(offset)?;
                }
                FDT_NOP => {
                    offset = offset.checked_add(4)?;
                }
                FDT_END => return None,
                _ => return None,
            }
        }
    }

    /// Reads one structure token from the validated structure block.
    ///
    /// # Parameters
    ///
    /// - `offset`: Byte offset within the structure block.
    fn read_token(&self, offset: usize) -> Option<u32> {
        read_be_u32_from_slice(self.structure, offset)
    }

    /// Returns one NUL-terminated string from the strings block.
    ///
    /// # Parameters
    ///
    /// - `offset`: Byte offset within the strings block.
    fn string_at(&self, offset: usize) -> Option<&'a str> {
        let bytes = self.strings.get(offset..)?;
        let end = bytes.iter().position(|byte| *byte == 0)?;
        str::from_utf8(&bytes[..end]).ok()
    }

    /// Reads one NUL-terminated UTF-8 string from `bytes` at `offset`.
    ///
    /// # Parameters
    ///
    /// - `bytes`: Byte slice containing the C string.
    /// - `offset`: Starting byte offset within `bytes`.
    fn read_c_string<'b>(
        &self,
        bytes: &'b [u8],
        offset: usize,
    ) -> Option<(&'b str, usize)> {
        let rest = bytes.get(offset..)?;
        let end = rest.iter().position(|byte| *byte == 0)?;
        let name = str::from_utf8(&rest[..end]).ok()?;
        Some((name, offset + end + 1))
    }
}

/// Returns `value` rounded up to the next 4-byte boundary.
///
/// # Parameters
///
/// - `value`: Byte offset or length to align.
pub(crate) fn align4(value: usize) -> usize {
    (value + 3) & !3
}

/// Reads one big-endian 32-bit value from `bytes` at `offset`.
///
/// # Parameters
///
/// - `bytes`: Source byte slice.
/// - `offset`: Starting byte offset of the 32-bit value.
pub(crate) fn read_be_u32_from_slice(bytes: &[u8], offset: usize) -> Option<u32> {
    let word = bytes.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([word[0], word[1], word[2], word[3]]))
}

/// Reads one big-endian 64-bit value from `bytes` at `offset`.
///
/// # Parameters
///
/// - `bytes`: Source byte slice.
/// - `offset`: Starting byte offset of the 64-bit value.
fn read_be_u64_from_slice(bytes: &[u8], offset: usize) -> Option<u64> {
    let word = bytes.get(offset..offset + 8)?;
    Some(u64::from_be_bytes([
        word[0], word[1], word[2], word[3], word[4], word[5], word[6], word[7],
    ]))
}

/// Decodes one big-endian cell sequence into a 64-bit integer.
///
/// # Parameters
///
/// - `bytes`: Cell bytes to decode.
/// - `cells`: Number of 32-bit cells contained in `bytes`.
pub(crate) fn read_cells(bytes: &[u8], cells: usize) -> u64 {
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

/// Reads one big-endian 32-bit value directly from the FDT blob.
///
/// # Parameters
///
/// - `ptr_raw`: Raw pointer to the start of the FDT blob.
/// - `offset`: Starting byte offset of the 32-bit value.
fn read_be_u32(ptr_raw: *const u8, offset: usize) -> Option<u32> {
    let word_ptr = unsafe { ptr_raw.add(offset) };
    let bytes = unsafe { slice::from_raw_parts(word_ptr, 4) };
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Validates the raw pointer used to construct one FDT view.
///
/// # Parameters
///
/// - `ptr_raw`: Pointer to the start of the flattened device tree blob.
fn validate_dtb_pointer(ptr_raw: *const u8) -> Result<(), FdtError> {
    if ptr_raw.is_null() {
        return Err(FdtError::NullPointer);
    }

    if (ptr_raw as usize & 0x7) != 0 {
        return Err(FdtError::MisalignedPointer);
    }

    Ok(())
}