//! Device-tree boot object stub.
//!
//! This module is a placeholder for the future boot-oriented device-tree layer.
//! It is intentionally separate from the low-level flattened-device-tree parser
//! so boot methods can depend on one stable DTB object type.

use core::ptr;

use crate::dtb_read::{
    align4, Fdt, FdtError, FdtHeader, FDT_BEGIN_NODE, FDT_END, FDT_END_NODE,
    FDT_NOP, FDT_PROP,
};
use crate::memory::{
    EFI_ALLOCATE_TYPE, EFI_MEMORY_TYPE, EFI_PAGE_SIZE, MemoryError,
    PageAllocator,
};

/// Errors returned while constructing a boot-oriented DTB object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DtbError {
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
    /// Low-level FDT validation or parsing failed.
    Fdt(FdtError),
}

/// Located property record within the structure block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PropertyLocation {
    /// Absolute byte offset of the property's `FDT_PROP` token.
    property_offset: usize,
    /// Total encoded property length, including token, header, and padding.
    total_length: usize,
}

/// Maximum encoded byte length for one string property including the trailing
/// zero byte.
const MAX_PROPERTY_STRING_LEN: usize = 256;

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
        let total_size = unsafe { Fdt::from_ptr(pointer) }
            .map_err(DtbError::from)?
            .total_size_bytes();

        Ok(Self {
            pointer,
            size: total_size,
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
        // SAFETY: `AllocatePages()` returned `allocated_size` writable bytes at
        // `cloned_pointer`. The source blob is valid for `header_total_size`
        // bytes, the destination is distinct from the source allocation, and
        // the trailing zero fill stays within the allocated range.
        unsafe {
            ptr::copy_nonoverlapping(self.pointer, cloned_pointer, header_total_size);
            ptr::write_bytes(
                cloned_pointer.add(header_total_size),
                0,
                allocated_size - header_total_size,
            );
            (&mut *(cloned_pointer as *mut FdtHeader))
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
    fn header(&self) -> &FdtHeader {
        // SAFETY: All `Dtb` values are constructed from pointers validated by
        // `from_ptr()` and continue to point at a DTB header for their lifetime.
        unsafe { &*(self.pointer as *const FdtHeader) }
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
        // SAFETY: `self.pointer` references a DTB allocation of at least the
        // header totalsize bytes, validated during construction and clone.
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

        let root = self.fdt_view()?.root_node().ok_or(DtbError::BadStructure)?;
        let mut parent_begin_offset = root.begin_offset();
        let mut parent_end_offset = root.end_offset();
        for component in components {
            if component.is_empty() {
                return Err(DtbError::InvalidPath);
            }

            let child_offsets = {
                let fdt = self.fdt_view()?;
                let parent = fdt
                    .node_at_offset(parent_begin_offset)
                    .ok_or(DtbError::BadStructure)?;
                fdt.find_child(parent, component)
                    .map(|child| (child.begin_offset(), child.end_offset()))
            };

            match child_offsets {
                Some((child_begin_offset, child_end_offset)) => {
                    parent_begin_offset = child_begin_offset;
                    parent_end_offset = child_end_offset;
                }
                None => {
                    let (child_begin_offset, child_end_offset) =
                        self.insert_child_node(parent_end_offset, component)?;
                    parent_begin_offset = child_begin_offset;
                    parent_end_offset = child_end_offset;
                }
            }
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
        self.set_property_bytes(node_path, property_name, &value.to_be_bytes())
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
        self.set_property_bytes(node_path, property_name, &value.to_be_bytes())
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
        let mut property_bytes = [0u8; MAX_PROPERTY_STRING_LEN];
        let property_len = value
            .len()
            .checked_add(1)
            .ok_or(DtbError::SizeOverflow)?;
        if property_len > property_bytes.len() {
            return Err(DtbError::BufferTooSmall);
        }

        property_bytes[..value.len()].copy_from_slice(value.as_bytes());
        self.set_property_bytes(node_path, property_name, &property_bytes[..property_len])
    }

    /// Inserts one 32-bit big-endian value into the DTB buffer.
    ///
    /// # Parameters
    ///
    /// - `offset`: Byte offset where the encoded value should be written.
    /// - `value`: 32-bit value to encode and insert.
    fn insert_u32(
        &mut self,
        offset: usize,
        value: u32,
    ) -> Result<usize, DtbError> {
        let bytes = value.to_be_bytes();
        self.insert_bytes(offset, &bytes)
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

        // SAFETY: `end <= self.size` guarantees the destination range lies
        // within the DTB allocation, and the source slice is valid for
        // `bytes.len()` readable bytes.
        unsafe {
            ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.pointer.cast_mut().add(offset),
                bytes.len(),
            );
        }

        Ok(end)
    }

    /// Creates or replaces one property payload exactly as provided.
    ///
    /// # Parameters
    ///
    /// - `node_path`: Absolute device-tree path of the target node.
    /// - `property_name`: Name of the property to create or replace.
    /// - `value`: Property payload bytes.
    pub fn set_property_bytes(
        &mut self,
        node_path: &str,
        property_name: &str,
        value: &[u8],
    ) -> Result<(), DtbError> {
        let node = self.find_node_location(node_path)?;
        let name_offset = self.find_or_add_string(property_name)?;
        let record_offset = self.prepare_property_record(node, property_name, value.len())?;

        self.insert_u32(record_offset, FDT_PROP)?;
        let stored_length =
            u32::try_from(value.len()).map_err(|_| DtbError::SizeOverflow)?;
        self.insert_u32(record_offset + 4, stored_length)?;
        self.insert_u32(record_offset + 8, name_offset)?;
        self.insert_bytes(record_offset + 12, value)?;
        self.zero_property_padding(record_offset, value.len())?;
        Ok(())
    }

    /// Returns one transient read-only FDT view over the current blob.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn fdt_view(&self) -> Result<Fdt<'_>, DtbError> {
        unsafe { Fdt::from_ptr(self.pointer) }.map_err(DtbError::from)
    }

    /// Resolves one absolute node path to its current structure-block node.
    ///
    /// # Parameters
    ///
    /// - `path`: Absolute device-tree path to resolve.
    fn find_node_location(&self, path: &str) -> Result<(usize, usize), DtbError> {
        if path == "/" {
            let root = self.fdt_view()?.root_node().ok_or(DtbError::BadStructure)?;
            return Ok((root.begin_offset(), root.end_offset()));
        }

        let mut components = path.split('/');
        if components.next() != Some("") {
            return Err(DtbError::InvalidPath);
        }

        for component in components.clone() {
            if component.is_empty() {
                return Err(DtbError::InvalidPath);
            }
        }

        let node = self.fdt_view()?.find_node(path).ok_or(DtbError::NodeNotFound)?;
        Ok((node.begin_offset(), node.end_offset()))
    }

    /// Inserts one new empty child node under `parent`.
    ///
    /// # Parameters
    ///
    /// - `parent`: Parent node receiving the new child.
    /// - `name`: Name of the new child node.
    fn insert_child_node(
        &mut self,
        parent_end_offset: usize,
        name: &str,
    ) -> Result<(usize, usize), DtbError> {
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

        let insertion_offset = parent_end_offset;
        self.splice_struct(
            self.structure_offset_in_blob(insertion_offset),
            0,
            &node_bytes[..end_token_offset + 4],
        )?;

        let inserted_node = self
            .fdt_view()?
            .node_at_offset(insertion_offset)
            .ok_or(DtbError::BadStructure)?;
        Ok((
            inserted_node.begin_offset(),
            inserted_node.end_offset(),
        ))
    }

    /// Finds one direct property under `node` by name.
    ///
    /// # Parameters
    ///
    /// - `node`: Node whose immediate properties should be searched.
    /// - `property_name`: Property name to find.
    fn find_direct_property(
        &self,
        node: (usize, usize),
        property_name: &str,
    ) -> Result<Option<PropertyLocation>, DtbError> {
        let fdt = self.fdt_view()?;
        let mut offset = fdt
            .after_begin_node(node.0)
            .ok_or(DtbError::BadStructure)?;

        while offset < node.1 {
            let token = fdt.read_token(offset).ok_or(DtbError::BadStructure)?;
            match token {
                FDT_PROP => {
                    let value_length =
                        fdt.read_token(offset + 4).ok_or(DtbError::BadStructure)? as usize;
                    let name_offset =
                        fdt.read_token(offset + 8).ok_or(DtbError::BadStructure)?;
                    let total_length = property_record_length(value_length)?;
                    if fdt.string_at(name_offset as usize).ok_or(DtbError::BadStructure)?
                        == property_name
                    {
                        return Ok(Some(PropertyLocation {
                            property_offset: self.structure_offset_in_blob(offset),
                            total_length,
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

    /// Ensures that the structure block contains one property record of the
    /// requested size and returns its starting offset.
    ///
    /// # Parameters
    ///
    /// - `node`: Node that will own the property.
    /// - `property_name`: Property name to replace or insert.
    /// - `value_length`: Final property payload length in bytes.
    fn prepare_property_record(
        &mut self,
        node: (usize, usize),
        property_name: &str,
        value_length: usize,
    ) -> Result<usize, DtbError> {
        match self.find_direct_property(node, property_name)? {
            Some(existing) => {
                let record_length = property_record_length(value_length)?;
                self.splice_struct_placeholder(
                    existing.property_offset,
                    existing.total_length,
                    record_length,
                )?;
                Ok(existing.property_offset)
            }
            None => {
                let insertion_offset = self.property_insertion_offset(node)?;
                let record_length = property_record_length(value_length)?;
                self.splice_struct_placeholder(insertion_offset, 0, record_length)?;
                Ok(insertion_offset)
            }
        }
    }

    /// Returns the insertion point for a new direct property.
    ///
    /// # Parameters
    ///
    /// - `node`: Node that will receive the new property.
    fn property_insertion_offset(
        &self,
        node: (usize, usize),
    ) -> Result<usize, DtbError> {
        let fdt = self.fdt_view()?;
        let mut offset = fdt
            .after_begin_node(node.0)
            .ok_or(DtbError::BadStructure)?;

        if offset == node.1 {
            return Ok(self.structure_offset_in_blob(node.1));
        }

        while offset < node.1 {
            let token = fdt.read_token(offset).ok_or(DtbError::BadStructure)?;
            match token {
                FDT_PROP => {
                    offset = fdt.after_property(offset).ok_or(DtbError::BadStructure)?;
                }
                FDT_NOP => {
                    offset += 4;
                }
                FDT_BEGIN_NODE | FDT_END_NODE => {
                    return Ok(self.structure_offset_in_blob(offset));
                }
                FDT_END => return Err(DtbError::BadStructure),
                _ => return Err(DtbError::BadStructure),
            }
        }

        Ok(self.structure_offset_in_blob(node.1))
    }

    /// Finds an existing property-name string or appends one to the strings block.
    ///
    /// # Parameters
    ///
    /// - `name`: Property name string to find or append.
    fn find_or_add_string(&mut self, name: &str) -> Result<u32, DtbError> {
        let strings_start = self.header().off_dt_strings() as usize;
        let strings_size = self.header().size_dt_strings() as usize;
        let mut offset = 0usize;
        {
            let bytes = self.blob_bytes();
            while offset < strings_size {
                let absolute_offset = strings_start + offset;
                let mut cursor = absolute_offset;
                while cursor < strings_start + strings_size {
                    if bytes[cursor] == 0 {
                        let existing = core::str::from_utf8(
                            &bytes[absolute_offset..cursor],
                        )
                        .map_err(|_| DtbError::BadStructure)?;
                        if existing == name {
                            return u32::try_from(offset)
                                .map_err(|_| DtbError::SizeOverflow);
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

        // SAFETY: The DTB header lives at the start of `self.pointer`, and the
        // strings-size field is updated only after the appended bytes were
        // written successfully within the allocation.
        let header = unsafe { &mut *(self.pointer.cast_mut() as *mut FdtHeader) };
        header.set_size_dt_strings(
            u32::try_from(strings_size + append_length)
                .map_err(|_| DtbError::SizeOverflow)?,
        );

        u32::try_from(append_offset).map_err(|_| DtbError::SizeOverflow)
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

        // SAFETY: All computed offsets are checked against the active DTB
        // bounds above. `ptr::copy` is used because the moved ranges may
        // overlap during in-place growth or shrink, and the zero fill stays
        // within the reserved replacement region.
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
        // SAFETY: The DTB header is stored at the start of the allocation and
        // is updated atomically after the structure splice succeeds.
        let header = unsafe { &mut *(self.pointer.cast_mut() as *mut FdtHeader) };
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
        // SAFETY: `ensure_range()` guarantees the padding range lies within the
        // DTB allocation and may be zeroed in place.
        unsafe {
            ptr::write_bytes(self.pointer.cast_mut().add(padding_offset), 0, padding);
        }
        Ok(())
    }

    /// Returns the absolute end offset of the structure block.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn structure_end_offset(&self) -> usize {
        self.header().off_dt_struct() as usize + self.header().size_dt_struct() as usize
    }

    /// Converts one structure-block-relative offset into a blob-absolute
    /// offset.
    ///
    /// # Parameters
    ///
    /// - `offset`: Byte offset relative to the start of the structure block.
    fn structure_offset_in_blob(&self, offset: usize) -> usize {
        self.header().off_dt_struct() as usize + offset
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
        // SAFETY: `self.pointer` is the base of a DTB allocation tracked by
        // `self.size`, so constructing an immutable slice of that span is valid.
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

impl From<FdtError> for DtbError {
    /// Converts FDT validation failures into boot-oriented DTB errors.
    ///
    /// # Parameters
    ///
    /// - `error`: Reader-side validation error to wrap.
    fn from(error: FdtError) -> Self {
        Self::Fdt(error)
    }
}