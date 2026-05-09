//! Device-tree boot object stub.
//!
//! This module is a placeholder for the future boot-oriented device-tree layer.
//! It is intentionally separate from the low-level flattened-device-tree parser
//! so boot methods can depend on one stable DTB object type.

use core::ptr;

use crate::memory::{
    EFI_ALLOCATE_TYPE, EFI_MEMORY_TYPE, EFI_PAGE_SIZE, MemoryError,
    PageAllocator,
};

/// Flattened device-tree header magic value.
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

/// Errors returned while constructing a boot-oriented DTB object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DtbError {
    /// The supplied device-tree pointer was null.
    NullPointer,
    /// The supplied device-tree pointer was not 8-byte aligned.
    MisalignedPointer,
    /// The device-tree header did not contain the expected magic value.
    BadMagic,
    /// The requested clone size was not larger than the header totalsize.
    InvalidCloneSize,
    /// The requested or allocated clone size could not fit in the header.
    SizeOverflow,
    /// The requested write did not fit inside the DTB buffer.
    BufferTooSmall,
    /// The supplied node path was not a valid absolute device-tree path.
    InvalidPath,
    /// The requested node could not be found in the structure block.
    NodeNotFound,
    /// The structure block contents were malformed.
    BadStructure,
    /// Allocating pages for the cloned DTB failed.
    Memory(MemoryError),
}

/// Fixed-size flattened device-tree header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct DtbHeader {
    /// Flattened device-tree magic number in big-endian form.
    magic: u32,
    /// Total blob size in bytes in big-endian form.
    totalsize: u32,
    /// Offset of the structure block in big-endian form.
    off_dt_struct: u32,
    /// Offset of the strings block in big-endian form.
    off_dt_strings: u32,
    /// Offset of the memory-reservation block in big-endian form.
    off_mem_rsvmap: u32,
    /// Device-tree format version in big-endian form.
    version: u32,
    /// Last compatible device-tree format version in big-endian form.
    last_comp_version: u32,
    /// Physical boot CPU identifier in big-endian form.
    boot_cpuid_phys: u32,
    /// Size of the strings block in big-endian form.
    size_dt_strings: u32,
    /// Size of the structure block in big-endian form.
    size_dt_struct: u32,
}

impl DtbHeader {
    /// Returns the decoded device-tree magic number.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn magic(&self) -> u32 {
        u32::from_be(self.magic)
    }

    /// Returns the decoded total blob size in bytes.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn total_size(&self) -> u32 {
        u32::from_be(self.totalsize)
    }

    /// Stores the encoded total blob size in bytes.
    ///
    /// # Parameters
    ///
    /// - `size`: Total blob size to encode into the header.
    fn set_total_size(&mut self, size: u32) {
        self.totalsize = size.to_be();
    }

    /// Returns the decoded structure-block offset.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn off_dt_struct(&self) -> u32 {
        u32::from_be(self.off_dt_struct)
    }

    /// Returns the decoded strings-block offset.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn off_dt_strings(&self) -> u32 {
        u32::from_be(self.off_dt_strings)
    }

    /// Returns the decoded memory-reservation-block offset.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn off_mem_rsvmap(&self) -> u32 {
        u32::from_be(self.off_mem_rsvmap)
    }

    /// Returns the decoded device-tree format version.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn version(&self) -> u32 {
        u32::from_be(self.version)
    }

    /// Returns the decoded last compatible format version.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn last_comp_version(&self) -> u32 {
        u32::from_be(self.last_comp_version)
    }

    /// Returns the decoded physical boot CPU identifier.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn boot_cpuid_phys(&self) -> u32 {
        u32::from_be(self.boot_cpuid_phys)
    }

    /// Returns the decoded strings-block size in bytes.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn size_dt_strings(&self) -> u32 {
        u32::from_be(self.size_dt_strings)
    }

    /// Returns the decoded structure-block size in bytes.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn size_dt_struct(&self) -> u32 {
        u32::from_be(self.size_dt_struct)
    }

    /// Stores the encoded strings-block offset.
    ///
    /// # Parameters
    ///
    /// - `offset`: Strings-block offset to encode into the header.
    fn set_off_dt_strings(&mut self, offset: u32) {
        self.off_dt_strings = offset.to_be();
    }

    /// Stores the encoded structure-block size.
    ///
    /// # Parameters
    ///
    /// - `size`: Structure-block size to encode into the header.
    fn set_size_dt_struct(&mut self, size: u32) {
        self.size_dt_struct = size.to_be();
    }

    /// Stores the encoded strings-block size.
    ///
    /// # Parameters
    ///
    /// - `size`: Strings-block size to encode into the header.
    fn set_size_dt_strings(&mut self, size: u32) {
        self.size_dt_strings = size.to_be();
    }
}

/// Located node range within the structure block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NodeLocation {
    /// Absolute byte offset of the node's `FDT_BEGIN_NODE` token.
    begin_offset: usize,
    /// Absolute byte offset of the node's matching `FDT_END_NODE` token.
    end_offset: usize,
}

/// Located property record within the structure block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PropertyLocation {
    /// Absolute byte offset of the property's `FDT_PROP` token.
    property_offset: usize,
    /// Absolute byte offset of the property's value payload.
    value_offset: usize,
    /// Length of the property's value payload in bytes.
    value_length: usize,
    /// Total encoded property length, including token, header, and padding.
    total_length: usize,
    /// Offset of the property name within the strings block.
    name_offset: u32,
}

/// Boot-oriented device-tree object passed to boot methods.
pub struct Dtb {
    /// Pointer to the start of the device-tree blob.
    pointer: *const u8,
    /// Size in bytes of the device-tree blob.
    size: usize,
}

impl Dtb {
    /// Creates one DTB object from a raw pointer.
    ///
    /// # Parameters
    ///
    /// - `pointer`: Pointer to the start of the device-tree blob.
    pub fn from_ptr(pointer: *const u8) -> Result<Self, DtbError> {
        validate_dtb_pointer(pointer)?;
        let header = unsafe { &*(pointer as *const DtbHeader) };

        Ok(Self {
            pointer,
            size: header.total_size() as usize,
        })
    }

    /// Clones this DTB into newly allocated memory with a larger total size.
    ///
    /// # Parameters
    ///
    /// - `new_size`: Minimum size in bytes requested for the cloned DTB.
    /// - `allocator`: Page allocator used to allocate storage for the clone.
    pub fn clone(
        &self,
        new_size: usize,
        allocator: &mut PageAllocator<'_>,
    ) -> Result<Self, DtbError> {
        let header_total_size = self.header().total_size() as usize;
        if new_size <= header_total_size {
            return Err(DtbError::InvalidCloneSize);
        }

        let page_size = EFI_PAGE_SIZE as usize;
        let page_count = new_size.div_ceil(page_size);
        let allocated_size = page_count
            .checked_mul(page_size)
            .ok_or(DtbError::SizeOverflow)?;
        let allocated_total_size =
            u32::try_from(allocated_size).map_err(|_| DtbError::SizeOverflow)?;

        let mut physical_start = 0;
        allocator
            .AllocatePages(
                EFI_ALLOCATE_TYPE::AllocateAnyPages,
                EFI_MEMORY_TYPE::EfiACPIReclaimMemory,
                page_count,
                &mut physical_start,
            )
            .map_err(DtbError::Memory)?;

        let cloned_pointer = physical_start as *mut u8;
        unsafe {
            ptr::copy_nonoverlapping(self.pointer, cloned_pointer, header_total_size);
            ptr::write_bytes(
                cloned_pointer.add(header_total_size),
                0,
                allocated_size - header_total_size,
            );
            (&mut *(cloned_pointer as *mut DtbHeader))
                .set_total_size(allocated_total_size);
        }

        Ok(Self {
            pointer: cloned_pointer.cast_const(),
            size: allocated_size,
        })
    }

    /// Returns the fixed-size device-tree header.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn header(&self) -> &DtbHeader {
        unsafe { &*(self.pointer as *const DtbHeader) }
    }

    /// Returns the pointer to the start of the device-tree blob.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn pointer(&self) -> *const u8 {
        self.pointer
    }

    /// Returns the decoded size of the device-tree blob in bytes.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Returns the current DTB bytes up to the header totalsize field.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(
                self.pointer,
                self.header().total_size() as usize,
            )
        }
    }

    /// Creates one node path in the structure block, adding missing components.
    ///
    /// # Parameters
    ///
    /// - `path`: Absolute device-tree node path to create.
    pub fn create_node(&mut self, path: &str) -> Result<(), DtbError> {
        let mut components = path.split('/');
        if components.next() != Some("") {
            return Err(DtbError::InvalidPath);
        }

        let root = self.root_node()?;
        let mut parent = root;
        for component in components {
            if component.is_empty() {
                return Err(DtbError::InvalidPath);
            }

            parent = match self.find_child(parent, component)? {
                Some(child) => child,
                None => self.insert_child_node(parent, component)?,
            };
        }

        Ok(())
    }

    /// Creates or replaces one 32-bit property on an existing node.
    ///
    /// # Parameters
    ///
    /// - `node_path`: Absolute device-tree path of the target node.
    /// - `property_name`: Name of the property to create or replace.
    /// - `value`: 32-bit property value encoded as one big-endian cell.
    pub fn set_property_u32(
        &mut self,
        node_path: &str,
        property_name: &str,
        value: u32,
    ) -> Result<(), DtbError> {
        self.set_property(node_path, property_name, &value.to_be_bytes())
    }

    /// Creates or replaces one 64-bit property on an existing node.
    ///
    /// # Parameters
    ///
    /// - `node_path`: Absolute device-tree path of the target node.
    /// - `property_name`: Name of the property to create or replace.
    /// - `value`: 64-bit property value encoded as two big-endian cells.
    pub fn set_property_u64(
        &mut self,
        node_path: &str,
        property_name: &str,
        value: u64,
    ) -> Result<(), DtbError> {
        self.set_property(node_path, property_name, &value.to_be_bytes())
    }

    /// Creates or replaces one zero-terminated string property.
    ///
    /// # Parameters
    ///
    /// - `node_path`: Absolute device-tree path of the target node.
    /// - `property_name`: Name of the property to create or replace.
    /// - `value`: UTF-8 string payload written with one trailing zero byte.
    pub fn set_property_string(
        &mut self,
        node_path: &str,
        property_name: &str,
        value: &str,
    ) -> Result<(), DtbError> {
        self.set_property_with_zero(node_path, property_name, value.as_bytes())
    }

    /// Inserts one 32-bit big-endian value into the DTB buffer.
    ///
    /// # Parameters
    ///
    /// - `offset`: Byte offset where the encoded value should be written.
    /// - `value`: 32-bit value to encode and insert.
    pub fn insert_u32(
        &mut self,
        offset: usize,
        value: u32,
    ) -> Result<usize, DtbError> {
        let bytes = value.to_be_bytes();
        self.insert_bytes(offset, &bytes)
    }

    /// Inserts one 64-bit big-endian value into the DTB buffer.
    ///
    /// # Parameters
    ///
    /// - `offset`: Byte offset where the encoded value should be written.
    /// - `value`: 64-bit value to encode and insert.
    pub fn insert_u64(
        &mut self,
        offset: usize,
        value: u64,
    ) -> Result<usize, DtbError> {
        let bytes = value.to_be_bytes();
        self.insert_bytes(offset, &bytes)
    }

    /// Inserts one zero-terminated UTF-8 string into the DTB buffer.
    ///
    /// # Parameters
    ///
    /// - `offset`: Byte offset where the string should be written.
    /// - `value`: UTF-8 string value to insert, followed by one zero byte.
    pub fn insert_string(
        &mut self,
        offset: usize,
        value: &str,
    ) -> Result<usize, DtbError> {
        let next_offset = self.insert_bytes(offset, value.as_bytes())?;
        self.insert_bytes(next_offset, &[0])
    }

    /// Inserts raw bytes into the DTB buffer and returns the next byte offset.
    ///
    /// # Parameters
    ///
    /// - `offset`: Byte offset where the bytes should be written.
    /// - `bytes`: Raw byte sequence to copy into the DTB buffer.
    fn insert_bytes(
        &mut self,
        offset: usize,
        bytes: &[u8],
    ) -> Result<usize, DtbError> {
        let end = offset
            .checked_add(bytes.len())
            .ok_or(DtbError::SizeOverflow)?;
        if end > self.size {
            return Err(DtbError::BufferTooSmall);
        }

        unsafe {
            ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.pointer.cast_mut().add(offset),
                bytes.len(),
            );
        }

        Ok(end)
    }

    /// Creates or replaces one property with an appended zero byte.
    ///
    /// # Parameters
    ///
    /// - `node_path`: Absolute device-tree path of the target node.
    /// - `property_name`: Name of the property to create or replace.
    /// - `value`: Property payload written before the trailing zero byte.
    fn set_property_with_zero(
        &mut self,
        node_path: &str,
        property_name: &str,
        value: &[u8],
    ) -> Result<(), DtbError> {
        let node = self.find_node(node_path)?;
        let name_offset = self.find_or_add_string(property_name)?;
        let record_offset = match self.find_direct_property(node, property_name)? {
            Some(existing) => {
                let record_length = property_record_length(value.len() + 1)?;
                self.splice_struct_placeholder(
                    existing.property_offset,
                    existing.total_length,
                    record_length,
                )?;
                existing.property_offset
            }
            None => {
                let insertion_offset = self.property_insertion_offset(node)?;
                let record_length = property_record_length(value.len() + 1)?;
                self.splice_struct_placeholder(insertion_offset, 0, record_length)?;
                insertion_offset
            }
        };

        self.insert_u32(record_offset, FDT_PROP)?;
        self.insert_u32(record_offset + 4, (value.len() + 1) as u32)?;
        self.insert_u32(record_offset + 8, name_offset)?;
        self.insert_bytes(record_offset + 12, value)?;
        self.insert_bytes(record_offset + 12 + value.len(), &[0])?;
        self.zero_property_padding(record_offset, value.len() + 1)?;
        Ok(())
    }

    /// Creates or replaces one property payload exactly as provided.
    ///
    /// # Parameters
    ///
    /// - `node_path`: Absolute device-tree path of the target node.
    /// - `property_name`: Name of the property to create or replace.
    /// - `value`: Property payload bytes.
    fn set_property(
        &mut self,
        node_path: &str,
        property_name: &str,
        value: &[u8],
    ) -> Result<(), DtbError> {
        let node = self.find_node(node_path)?;
        let name_offset = self.find_or_add_string(property_name)?;
        let record_offset = match self.find_direct_property(node, property_name)? {
            Some(existing) => {
                let record_length = property_record_length(value.len())?;
                self.splice_struct_placeholder(
                    existing.property_offset,
                    existing.total_length,
                    record_length,
                )?;
                existing.property_offset
            }
            None => {
                let insertion_offset = self.property_insertion_offset(node)?;
                let record_length = property_record_length(value.len())?;
                self.splice_struct_placeholder(insertion_offset, 0, record_length)?;
                insertion_offset
            }
        };

        self.insert_u32(record_offset, FDT_PROP)?;
        self.insert_u32(record_offset + 4, value.len() as u32)?;
        self.insert_u32(record_offset + 8, name_offset)?;
        self.insert_bytes(record_offset + 12, value)?;
        self.zero_property_padding(record_offset, value.len())?;
        Ok(())
    }

    /// Returns the root node from the structure block.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn root_node(&self) -> Result<NodeLocation, DtbError> {
        let begin_offset = self.header().off_dt_struct() as usize;
        if self.read_token(begin_offset)? != FDT_BEGIN_NODE {
            return Err(DtbError::BadStructure);
        }

        Ok(NodeLocation {
            begin_offset,
            end_offset: self.node_end_offset(begin_offset)?,
        })
    }

    /// Finds one existing absolute node path.
    ///
    /// # Parameters
    ///
    /// - `path`: Absolute device-tree path to resolve.
    fn find_node(&self, path: &str) -> Result<NodeLocation, DtbError> {
        if path == "/" {
            return self.root_node();
        }

        let mut components = path.split('/');
        if components.next() != Some("") {
            return Err(DtbError::InvalidPath);
        }

        let mut node = self.root_node()?;
        for component in components {
            if component.is_empty() {
                return Err(DtbError::InvalidPath);
            }

            node = self
                .find_child(node, component)?
                .ok_or(DtbError::NodeNotFound)?;
        }

        Ok(node)
    }

    /// Finds one direct child node by name under `parent`.
    ///
    /// # Parameters
    ///
    /// - `parent`: Parent node to search under.
    /// - `name`: Direct child node name to find.
    fn find_child(
        &self,
        parent: NodeLocation,
        name: &str,
    ) -> Result<Option<NodeLocation>, DtbError> {
        let mut offset = self.after_begin_node(parent.begin_offset)?;
        let mut depth = 0usize;

        while offset < parent.end_offset {
            let token = self.read_token(offset)?;
            match token {
                FDT_BEGIN_NODE => {
                    let (node_name, next_offset) = self.read_node_name(offset)?;
                    if depth == 0 && node_name == name {
                        return Ok(Some(NodeLocation {
                            begin_offset: offset,
                            end_offset: self.node_end_offset(offset)?,
                        }));
                    }

                    depth += 1;
                    offset = next_offset;
                }
                FDT_END_NODE => {
                    if depth == 0 {
                        return Err(DtbError::BadStructure);
                    }

                    depth -= 1;
                    offset += 4;
                }
                FDT_PROP => {
                    offset = self.after_property(offset)?;
                }
                FDT_NOP => {
                    offset += 4;
                }
                FDT_END => return Err(DtbError::BadStructure),
                _ => return Err(DtbError::BadStructure),
            }
        }

        Ok(None)
    }

    /// Inserts one new empty child node under `parent`.
    ///
    /// # Parameters
    ///
    /// - `parent`: Parent node receiving the new child.
    /// - `name`: Name of the new child node.
    fn insert_child_node(
        &mut self,
        parent: NodeLocation,
        name: &str,
    ) -> Result<NodeLocation, DtbError> {
        let mut node_bytes = [0u8; 4 + 256 + 4 + 3];
        let required = 4usize
            .checked_add(name.len())
            .and_then(|value| value.checked_add(1))
            .and_then(|value| value.checked_add(3))
            .and_then(|value| value.checked_add(4))
            .ok_or(DtbError::SizeOverflow)?;
        if required > node_bytes.len() {
            return Err(DtbError::BufferTooSmall);
        }

        node_bytes[..4].copy_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
        node_bytes[4..4 + name.len()].copy_from_slice(name.as_bytes());
        node_bytes[4 + name.len()] = 0;
        let end_token_offset = align4(4 + name.len() + 1);
        node_bytes[end_token_offset..end_token_offset + 4]
            .copy_from_slice(&FDT_END_NODE.to_be_bytes());

        let insertion_offset = parent.end_offset;
        self.splice_struct(insertion_offset, 0, &node_bytes[..end_token_offset + 4])?;

        Ok(NodeLocation {
            begin_offset: insertion_offset,
            end_offset: insertion_offset + end_token_offset,
        })
    }

    /// Returns the absolute offset after one `FDT_BEGIN_NODE` record.
    ///
    /// # Parameters
    ///
    /// - `begin_offset`: Absolute offset of the `FDT_BEGIN_NODE` token.
    fn after_begin_node(&self, begin_offset: usize) -> Result<usize, DtbError> {
        let (_, next_offset) = self.read_node_name(begin_offset)?;
        Ok(next_offset)
    }

    /// Finds one direct property under `node` by name.
    ///
    /// # Parameters
    ///
    /// - `node`: Node whose immediate properties should be searched.
    /// - `property_name`: Property name to find.
    fn find_direct_property(
        &self,
        node: NodeLocation,
        property_name: &str,
    ) -> Result<Option<PropertyLocation>, DtbError> {
        let mut offset = self.after_begin_node(node.begin_offset)?;

        while offset < node.end_offset {
            let token = self.read_token(offset)?;
            match token {
                FDT_PROP => {
                    let value_length = self.read_token(offset + 4)? as usize;
                    let name_offset = self.read_token(offset + 8)?;
                    let value_offset = offset + 12;
                    let total_length = property_record_length(value_length)?;
                    if self.name_at(name_offset as usize)? == property_name {
                        return Ok(Some(PropertyLocation {
                            property_offset: offset,
                            value_offset,
                            value_length,
                            total_length,
                            name_offset,
                        }));
                    }
                    offset += total_length;
                }
                FDT_NOP => {
                    offset += 4;
                }
                FDT_BEGIN_NODE | FDT_END_NODE => return Ok(None),
                FDT_END => return Err(DtbError::BadStructure),
                _ => return Err(DtbError::BadStructure),
            }
        }

        Ok(None)
    }

    /// Returns the insertion point for a new direct property.
    ///
    /// # Parameters
    ///
    /// - `node`: Node that will receive the new property.
    fn property_insertion_offset(
        &self,
        node: NodeLocation,
    ) -> Result<usize, DtbError> {
        let mut offset = self.after_begin_node(node.begin_offset)?;

        if offset == node.end_offset {
            return Ok(node.end_offset);
        }

        while offset < node.end_offset {
            let token = self.read_token(offset)?;
            match token {
                FDT_PROP => {
                    offset = self.after_property(offset)?;
                }
                FDT_NOP => {
                    offset += 4;
                }
                FDT_BEGIN_NODE | FDT_END_NODE => return Ok(offset),
                FDT_END => return Err(DtbError::BadStructure),
                _ => return Err(DtbError::BadStructure),
            }
        }

        Ok(node.end_offset)
    }

    /// Returns the absolute offset after one property record.
    ///
    /// # Parameters
    ///
    /// - `property_offset`: Absolute offset of the `FDT_PROP` token.
    fn after_property(&self, property_offset: usize) -> Result<usize, DtbError> {
        let length = self.read_token(property_offset + 4)? as usize;
        let value_offset = property_offset + 12;
        let value_end = value_offset
            .checked_add(length)
            .ok_or(DtbError::SizeOverflow)?;
        self.ensure_range(value_offset, length)?;
        Ok(align4(value_end))
    }

    /// Reads one node name and returns the aligned next offset.
    ///
    /// # Parameters
    ///
    /// - `begin_offset`: Absolute offset of the `FDT_BEGIN_NODE` token.
    fn read_node_name<'a>(
        &'a self,
        begin_offset: usize,
    ) -> Result<(&'a str, usize), DtbError> {
        if self.read_token(begin_offset)? != FDT_BEGIN_NODE {
            return Err(DtbError::BadStructure);
        }

        let name_offset = begin_offset + 4;
        let structure_end = self.structure_end_offset();
        let bytes = self.blob_bytes();
        let mut cursor = name_offset;
        while cursor < structure_end {
            if bytes[cursor] == 0 {
                let name = core::str::from_utf8(&bytes[name_offset..cursor])
                    .map_err(|_| DtbError::BadStructure)?;
                return Ok((name, align4(cursor + 1)));
            }
            cursor += 1;
        }

        Err(DtbError::BadStructure)
    }

    /// Returns one property name from the strings block.
    ///
    /// # Parameters
    ///
    /// - `name_offset`: Offset within the strings block.
    fn name_at<'a>(&'a self, name_offset: usize) -> Result<&'a str, DtbError> {
        let strings_start = self.header().off_dt_strings() as usize;
        let strings_end = self.used_blob_end_offset();
        let absolute_offset = strings_start
            .checked_add(name_offset)
            .ok_or(DtbError::SizeOverflow)?;
        if absolute_offset >= strings_end {
            return Err(DtbError::BadStructure);
        }

        let bytes = self.blob_bytes();
        let mut cursor = absolute_offset;
        while cursor < strings_end {
            if bytes[cursor] == 0 {
                return core::str::from_utf8(&bytes[absolute_offset..cursor])
                    .map_err(|_| DtbError::BadStructure);
            }
            cursor += 1;
        }

        Err(DtbError::BadStructure)
    }

    /// Finds an existing property-name string or appends one to the strings block.
    ///
    /// # Parameters
    ///
    /// - `name`: Property name string to find or append.
    fn find_or_add_string(&mut self, name: &str) -> Result<u32, DtbError> {
        let strings_start = self.header().off_dt_strings() as usize;
        let strings_size = self.header().size_dt_strings() as usize;
        let bytes = self.blob_bytes();
        let mut offset = 0usize;
        while offset < strings_size {
            let absolute_offset = strings_start + offset;
            let mut cursor = absolute_offset;
            while cursor < strings_start + strings_size {
                if bytes[cursor] == 0 {
                    let existing = core::str::from_utf8(&bytes[absolute_offset..cursor])
                        .map_err(|_| DtbError::BadStructure)?;
                    if existing == name {
                        return u32::try_from(offset).map_err(|_| DtbError::SizeOverflow);
                    }
                    offset = cursor - strings_start + 1;
                    break;
                }
                cursor += 1;
            }

            if cursor == strings_start + strings_size {
                return Err(DtbError::BadStructure);
            }
        }

        let append_offset = strings_size;
        let append_length = name
            .len()
            .checked_add(1)
            .ok_or(DtbError::SizeOverflow)?;
        let absolute_offset = self.used_blob_end_offset();
        self.ensure_range(absolute_offset, append_length)?;
        self.insert_bytes(absolute_offset, name.as_bytes())?;
        self.insert_bytes(absolute_offset + name.len(), &[0])?;

        let header = unsafe { &mut *(self.pointer.cast_mut() as *mut DtbHeader) };
        header.set_size_dt_strings(
            u32::try_from(strings_size + append_length)
                .map_err(|_| DtbError::SizeOverflow)?,
        );

        u32::try_from(append_offset).map_err(|_| DtbError::SizeOverflow)
    }

    /// Finds the matching `FDT_END_NODE` token for one node.
    ///
    /// # Parameters
    ///
    /// - `begin_offset`: Absolute offset of the `FDT_BEGIN_NODE` token.
    fn node_end_offset(&self, begin_offset: usize) -> Result<usize, DtbError> {
        let mut offset = begin_offset;
        let mut depth = 0usize;

        loop {
            let token = self.read_token(offset)?;
            match token {
                FDT_BEGIN_NODE => {
                    depth += 1;
                    offset = self.after_begin_node(offset)?;
                }
                FDT_END_NODE => {
                    depth = depth.checked_sub(1).ok_or(DtbError::BadStructure)?;
                    if depth == 0 {
                        return Ok(offset);
                    }
                    offset += 4;
                }
                FDT_PROP => {
                    offset = self.after_property(offset)?;
                }
                FDT_NOP => {
                    offset += 4;
                }
                FDT_END => return Err(DtbError::BadStructure),
                _ => return Err(DtbError::BadStructure),
            }
        }
    }

    /// Inserts or removes bytes inside the structure block.
    ///
    /// # Parameters
    ///
    /// - `offset`: Absolute splice location in the blob.
    /// - `old_len`: Number of existing bytes to replace.
    /// - `new_bytes`: Replacement byte sequence.
    fn splice_struct(
        &mut self,
        offset: usize,
        old_len: usize,
        new_bytes: &[u8],
    ) -> Result<(), DtbError> {
        self.splice_struct_placeholder(offset, old_len, new_bytes.len())?;

        unsafe {
            ptr::copy_nonoverlapping(
                new_bytes.as_ptr(),
                self.pointer.cast_mut().add(offset),
                new_bytes.len(),
            );
        }

        Ok(())
    }

    /// Inserts or removes structure-block space without writing payload bytes.
    ///
    /// # Parameters
    ///
    /// - `offset`: Absolute splice location in the blob.
    /// - `old_len`: Number of existing bytes to replace.
    /// - `new_len`: Number of replacement bytes to reserve.
    fn splice_struct_placeholder(
        &mut self,
        offset: usize,
        old_len: usize,
        new_len: usize,
    ) -> Result<(), DtbError> {
        let used_end = self.used_blob_end_offset();
        let struct_start = self.header().off_dt_struct() as usize;
        let struct_end = self.structure_end_offset();
        if offset < struct_start || offset > struct_end {
            return Err(DtbError::BadStructure);
        }
        if offset.checked_add(old_len).ok_or(DtbError::SizeOverflow)? > struct_end {
            return Err(DtbError::BadStructure);
        }

        let resulting_used_end = used_end
            .checked_sub(old_len)
            .and_then(|value| value.checked_add(new_len))
            .ok_or(DtbError::SizeOverflow)?;
        if resulting_used_end > self.size {
            return Err(DtbError::BufferTooSmall);
        }

        unsafe {
            let base = self.pointer.cast_mut();
            ptr::copy(
                base.add(offset + old_len),
                base.add(offset + new_len),
                used_end - (offset + old_len),
            );
            ptr::write_bytes(base.add(offset), 0, new_len);
        }

        let delta = new_len as isize - old_len as isize;
        let delta_i32 = i32::try_from(delta).map_err(|_| DtbError::SizeOverflow)?;
        let header = unsafe { &mut *(self.pointer.cast_mut() as *mut DtbHeader) };
        header.set_size_dt_struct(
            header
                .size_dt_struct()
            .checked_add_signed(delta_i32)
                .ok_or(DtbError::SizeOverflow)?,
        );
        header.set_off_dt_strings(
            header
                .off_dt_strings()
            .checked_add_signed(delta_i32)
                .ok_or(DtbError::SizeOverflow)?,
        );

        Ok(())
    }

    /// Clears the padding bytes after one property value.
    ///
    /// # Parameters
    ///
    /// - `property_offset`: Absolute offset of the `FDT_PROP` token.
    /// - `value_length`: Actual property value length in bytes.
    fn zero_property_padding(
        &mut self,
        property_offset: usize,
        value_length: usize,
    ) -> Result<(), DtbError> {
        let padded_length = align4(value_length);
        let padding = padded_length - value_length;
        if padding == 0 {
            return Ok(());
        }

        let padding_offset = property_offset + 12 + value_length;
        self.ensure_range(padding_offset, padding)?;
        unsafe {
            ptr::write_bytes(self.pointer.cast_mut().add(padding_offset), 0, padding);
        }
        Ok(())
    }

    /// Reads one structure token from the blob.
    ///
    /// # Parameters
    ///
    /// - `offset`: Absolute offset of the token to read.
    fn read_token(&self, offset: usize) -> Result<u32, DtbError> {
        self.ensure_range(offset, 4)?;
        let bytes = self.blob_bytes();
        Ok(u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]))
    }

    /// Returns the absolute end offset of the structure block.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn structure_end_offset(&self) -> usize {
        self.header().off_dt_struct() as usize + self.header().size_dt_struct() as usize
    }

    /// Returns the absolute end offset of the used blob contents.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn used_blob_end_offset(&self) -> usize {
        self.header().off_dt_strings() as usize + self.header().size_dt_strings() as usize
    }

    /// Returns the blob as one immutable byte slice.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn blob_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.pointer, self.size) }
    }

    /// Checks that a byte range fits inside the DTB allocation.
    ///
    /// # Parameters
    ///
    /// - `offset`: Starting byte offset of the range.
    /// - `length`: Length in bytes of the range.
    fn ensure_range(&self, offset: usize, length: usize) -> Result<(), DtbError> {
        let end = offset.checked_add(length).ok_or(DtbError::SizeOverflow)?;
        if end > self.size {
            return Err(DtbError::BufferTooSmall);
        }

        Ok(())
    }
}

/// Returns `value` rounded up to the next 4-byte boundary.
///
/// # Parameters
///
/// - `value`: Byte offset or length to align.
fn align4(value: usize) -> usize {
    (value + 3) & !3
}

/// Returns the aligned encoded length of one property record.
///
/// # Parameters
///
/// - `value_length`: Length in bytes of the property value payload.
fn property_record_length(value_length: usize) -> Result<usize, DtbError> {
    12usize
        .checked_add(align4(value_length))
        .ok_or(DtbError::SizeOverflow)
}

impl From<MemoryError> for DtbError {
    /// Converts page-allocation failures into DTB construction failures.
    ///
    /// # Parameters
    ///
    /// - `error`: Memory-layer error to wrap.
    fn from(error: MemoryError) -> Self {
        Self::Memory(error)
    }
}

/// Validates the raw pointer used to construct one DTB object.
///
/// # Parameters
///
/// - `pointer`: Pointer to the start of the device-tree blob.
fn validate_dtb_pointer(pointer: *const u8) -> Result<(), DtbError> {
    if pointer.is_null() {
        return Err(DtbError::NullPointer);
    }

    if (pointer as usize & 0x7) != 0 {
        return Err(DtbError::MisalignedPointer);
    }

    let header = unsafe { &*(pointer as *const DtbHeader) };
    if header.magic() != FDT_MAGIC {
        return Err(DtbError::BadMagic);
    }

    Ok(())
}