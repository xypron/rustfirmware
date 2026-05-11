#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

//! EFI-style page-based memory map management.
//!
//! This module builds a 4 KiB page map from device-tree RAM and reservation
//! ranges. All RAM discovered through `/memory` is first represented as EFI
//! conventional memory and then carved by reserved regions from the device-tree
//! reserve map and `/reserved-memory` subtree.

use core::arch::asm;
use core::cmp::{max, min};

use crate::dtb_read::{Fdt, MemoryRegion};

unsafe extern "C" {
    /// Linker-defined start of the firmware text and rodata range.
    static __firmware_code_start: u8;
    /// Linker-defined end of the firmware text and rodata range.
    static __firmware_code_end: u8;
    /// Linker-defined start of the firmware writable data range.
    static __firmware_data_start: u8;
    /// Linker-defined end of the firmware writable data range.
    static __firmware_data_end: u8;
    /// Linker-defined start of the firmware heap range.
    static __heap_start: u8;
    /// Linker-defined end of the firmware heap range.
    static __heap_end: u8;
    /// Linker-defined bottom of the firmware stack range.
    static __stack_bottom: u8;
    /// Linker-defined top of the firmware stack range.
    static __stack_top: u8;
}

/// Number of address bits covered by one EFI page.
pub const EFI_PAGE_SHIFT: usize = 12;
/// Size in bytes of one EFI page.
pub const EFI_PAGE_SIZE: UINT64 = 1 << EFI_PAGE_SHIFT;
/// EFI memory descriptor version emitted by this module.
pub const EFI_MEMORY_DESCRIPTOR_VERSION: UINT32 = 1;

/// EFI `UINT32` equivalent.
pub type UINT32 = u32;
/// EFI `UINT64` equivalent.
pub type UINT64 = u64;
/// EFI `UINTN` equivalent on this target.
pub type UINTN = usize;
/// EFI physical address type.
pub type EFI_PHYSICAL_ADDRESS = u64;
/// EFI virtual address type.
pub type EFI_VIRTUAL_ADDRESS = u64;

/// EFI memory types used in memory descriptors and page allocation requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum EFI_MEMORY_TYPE {
    /// Reserved memory that must not be allocated.
    EfiReservedMemoryType = 0,
    /// Loader code pages.
    EfiLoaderCode = 1,
    /// Loader data pages.
    EfiLoaderData = 2,
    /// Boot-services code pages.
    EfiBootServicesCode = 3,
    /// Boot-services data pages.
    EfiBootServicesData = 4,
    /// Runtime-services code pages.
    EfiRuntimeServicesCode = 5,
    /// Runtime-services data pages.
    EfiRuntimeServicesData = 6,
    /// Unallocated conventional RAM.
    EfiConventionalMemory = 7,
    /// Faulty or otherwise unusable memory.
    EfiUnusableMemory = 8,
    /// ACPI reclaimable memory.
    EfiACPIReclaimMemory = 9,
    /// ACPI non-volatile storage memory.
    EfiACPIMemoryNVS = 10,
    /// Memory-mapped I/O space.
    EfiMemoryMappedIO = 11,
    /// Memory-mapped I/O port space.
    EfiMemoryMappedIOPortSpace = 12,
    /// Processor abstraction layer code.
    EfiPalCode = 13,
    /// Persistent memory.
    EfiPersistentMemory = 14,
    /// Unaccepted memory that requires explicit acceptance.
    EfiUnacceptedMemoryType = 15,
    /// Sentinel value that is not a usable descriptor type.
    EfiMaxMemoryType = 16,
}

/// EFI page allocation policies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum EFI_ALLOCATE_TYPE {
    /// Allocate any suitable pages.
    AllocateAnyPages = 0,
    /// Allocate pages below or at a maximum address.
    AllocateMaxAddress = 1,
    /// Allocate pages at the exact address supplied.
    AllocateAddress = 2,
    /// Sentinel value that is not a usable policy.
    MaxAllocateType = 3,
}

/// Search direction for aligned page allocations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocationDirection {
    /// Choose the lowest suitable aligned address.
    Low,
    /// Choose the highest suitable aligned address.
    High,
}

/// EFI memory descriptor fields carried in an EFI memory map.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct EFI_MEMORY_DESCRIPTOR {
    /// EFI memory type encoded as an `EFI_MEMORY_TYPE` discriminant.
    pub Type: UINT32,
    /// Physical base address of the described range.
    pub PhysicalStart: EFI_PHYSICAL_ADDRESS,
    /// Virtual base address assigned for runtime mappings.
    pub VirtualStart: EFI_VIRTUAL_ADDRESS,
    /// Number of 4 KiB pages covered by the range.
    pub NumberOfPages: UINT64,
    /// EFI memory attribute bitmask.
    pub Attribute: UINT64,
}

/// Errors returned while building or mutating the EFI-style page map.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryError {
    /// A caller supplied a parameter value that violates page-map invariants.
    InvalidParameter,
    /// The supplied descriptor buffer is too small for the requested splits.
    BufferTooSmall,
    /// The requested allocation could not be satisfied.
    OutOfResources,
    /// The requested range was not found in the current page map.
    NotFound,
    /// Address arithmetic overflowed while processing a region.
    AddressOverflow,
}

/// Page allocator backed by an in-place EFI memory descriptor array.
pub struct PageAllocator<'a> {
    /// Descriptor buffer containing the current EFI-style memory map.
    descriptors: &'a mut [EFI_MEMORY_DESCRIPTOR],
    /// Number of valid descriptors stored at the start of `descriptors`.
    descriptor_count: usize,
}

/// Empty descriptor value used for temporary EFI memory-map arrays.
pub const EMPTY_MEMORY_DESCRIPTOR: EFI_MEMORY_DESCRIPTOR =
    EFI_MEMORY_DESCRIPTOR {
        Type: 0,
        PhysicalStart: 0,
        VirtualStart: 0,
        NumberOfPages: 0,
        Attribute: 0,
    };

/// Builds a page allocator from the live boot-time device tree.
///
/// # Parameters
///
/// - `device_tree_ptr`: Pointer to the live flattened device tree.
/// - `memory_regions`: Scratch slice that receives `/memory` ranges.
/// - `reserved_regions`: Scratch slice that receives reserved ranges.
/// - `descriptors`: Descriptor buffer that receives the EFI-style memory map.
pub fn page_allocator_from_live_fdt<'a>(
    device_tree_ptr: *const u8,
    memory_regions: &mut [MemoryRegion],
    reserved_regions: &mut [MemoryRegion],
    descriptors: &'a mut [EFI_MEMORY_DESCRIPTOR],
) -> Option<PageAllocator<'a>> {
    let fdt = unsafe { Fdt::from_ptr(device_tree_ptr).ok()? };
    PageAllocator::from_fdt(
        &fdt,
        memory_regions,
        reserved_regions,
        descriptors,
    )
    .ok()
}

impl<'a> PageAllocator<'a> {
    /// Builds a page allocator from the RAM and reserved regions in an FDT.
    ///
    /// # Parameters
    ///
    /// - `fdt`: Flattened device tree supplying RAM and reservation ranges.
    /// - `memory_regions`: Scratch slice that receives `/memory` ranges.
    /// - `reserved_regions`: Scratch slice that receives reservation ranges.
    /// - `descriptors`: Descriptor buffer used for the EFI-style page map.
    pub fn from_fdt(
        fdt: &Fdt,
        memory_regions: &mut [MemoryRegion],
        reserved_regions: &mut [MemoryRegion],
        descriptors: &'a mut [EFI_MEMORY_DESCRIPTOR],
    ) -> Result<Self, MemoryError> {
        let memory_region_count = fdt.memory_regions(memory_regions);
        let reserved_region_count = fdt.reserved_regions(reserved_regions);
        let mut allocator = Self {
            descriptors,
            descriptor_count: 0,
        };

        for region in &memory_regions[..memory_region_count] {
            allocator.add_memory_region(*region)?;
        }

        allocator.coalesce();
        fdt.reserve_in(&mut allocator)?;

        for region in &reserved_regions[..reserved_region_count] {
            allocator.add_reserved_region(*region)?;
        }

        allocator.add_firmware_region(
            linker_region(
                firmware_code_start(),
                firmware_code_end(),
            )?,
            EFI_MEMORY_TYPE::EfiBootServicesCode,
        )?;
        allocator.add_firmware_region(
            linker_region(
                firmware_data_start(),
                firmware_data_end(),
            )?,
            EFI_MEMORY_TYPE::EfiBootServicesData,
        )?;
        allocator.add_firmware_region(
            linker_region(
                firmware_heap_start(),
                firmware_heap_end(),
            )?,
            EFI_MEMORY_TYPE::EfiBootServicesData,
        )?;
        allocator.add_firmware_region(
            linker_region(
                firmware_stack_bottom(),
                firmware_stack_top(),
            )?,
            EFI_MEMORY_TYPE::EfiBootServicesData,
        )?;

        allocator.coalesce();
        Ok(allocator)
    }

    /// Builds a page allocator from explicit RAM and reserved region lists.
    ///
    /// # Parameters
    ///
    /// - `memory_regions`: RAM ranges to seed as conventional memory.
    /// - `reserved_regions`: Reserved ranges to carve from the RAM map.
    /// - `descriptors`: Descriptor buffer used for the EFI-style page map.
    pub fn from_regions(
        memory_regions: &[MemoryRegion],
        reserved_regions: &[MemoryRegion],
        descriptors: &'a mut [EFI_MEMORY_DESCRIPTOR],
    ) -> Result<Self, MemoryError> {
        let mut allocator = Self {
            descriptors,
            descriptor_count: 0,
        };

        for region in memory_regions {
            allocator.add_memory_region(*region)?;
        }

        allocator.coalesce();

        for region in reserved_regions {
            allocator.add_reserved_region(*region)?;
        }

        allocator.add_firmware_region(
            linker_region(
                firmware_code_start(),
                firmware_code_end(),
            )?,
            EFI_MEMORY_TYPE::EfiBootServicesCode,
        )?;
        allocator.add_firmware_region(
            linker_region(
                firmware_data_start(),
                firmware_data_end(),
            )?,
            EFI_MEMORY_TYPE::EfiBootServicesData,
        )?;
        allocator.add_firmware_region(
            linker_region(
                firmware_heap_start(),
                firmware_heap_end(),
            )?,
            EFI_MEMORY_TYPE::EfiBootServicesData,
        )?;
        allocator.add_firmware_region(
            linker_region(
                firmware_stack_bottom(),
                firmware_stack_top(),
            )?,
            EFI_MEMORY_TYPE::EfiBootServicesData,
        )?;

        allocator.coalesce();
        Ok(allocator)
    }

    /// Returns the EFI memory descriptor version used by this allocator.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub const fn descriptor_version() -> UINT32 {
        EFI_MEMORY_DESCRIPTOR_VERSION
    }

    /// Returns the number of active descriptors in the page map.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub const fn descriptor_count(&self) -> usize {
        self.descriptor_count
    }

    /// Returns the active EFI memory descriptors.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn descriptors(&self) -> &[EFI_MEMORY_DESCRIPTOR] {
        &self.descriptors[..self.descriptor_count]
    }

    /// Reserves one region in the current EFI-style memory map.
    ///
    /// # Parameters
    ///
    /// - `region`: Region to carve from conventional memory.
    pub fn reserve_region(&mut self, region: MemoryRegion) -> Result<(), MemoryError> {
        self.add_reserved_region(region)?;
        self.coalesce();
        Ok(())
    }

    /// Allocates pages using EFI `AllocatePages` parameter names and types.
    ///
    /// # Parameters
    ///
    /// - `Type`: Allocation policy to apply.
    /// - `MemoryType`: EFI memory type assigned to the allocated range.
    /// - `Pages`: Number of 4 KiB pages to allocate.
    /// - `Memory`: Input/output physical address parameter used by EFI.
    pub fn AllocatePages(
        &mut self,
        Type: EFI_ALLOCATE_TYPE,
        MemoryType: EFI_MEMORY_TYPE,
        Pages: UINTN,
        Memory: &mut EFI_PHYSICAL_ADDRESS,
    ) -> Result<(), MemoryError> {
        if Pages == 0 || MemoryType == EFI_MEMORY_TYPE::EfiMaxMemoryType || Type == EFI_ALLOCATE_TYPE::MaxAllocateType {
            return Err(MemoryError::InvalidParameter);
        }

        let page_count = pages_to_u64(Pages)?;
        let byte_count = pages_to_bytes(Pages)?;

        let allocation_start = match Type {
            EFI_ALLOCATE_TYPE::AllocateAnyPages => self.find_allocation_any_pages(page_count)?,
            EFI_ALLOCATE_TYPE::AllocateMaxAddress => self.find_allocation_max_address(byte_count, *Memory)?,
            EFI_ALLOCATE_TYPE::AllocateAddress => {
                if !is_page_aligned(*Memory) {
                    return Err(MemoryError::InvalidParameter);
                }
                *Memory
            }
            EFI_ALLOCATE_TYPE::MaxAllocateType => return Err(MemoryError::InvalidParameter),
        };

        self.allocate_exact_range(allocation_start, page_count, MemoryType)?;
        *Memory = allocation_start;
        Ok(())
    }

    /// Allocates enough 4 KiB pages to cover `size_bytes` from high memory.
    ///
    /// # Parameters
    ///
    /// - `memory_type`: EFI memory type assigned to the allocated range.
    /// - `size_bytes`: Number of bytes the allocation must cover.
    pub fn allocate_pages_for_size(
        &mut self,
        memory_type: EFI_MEMORY_TYPE,
        size_bytes: UINTN,
    ) -> Result<EFI_PHYSICAL_ADDRESS, MemoryError> {
        if memory_type == EFI_MEMORY_TYPE::EfiMaxMemoryType {
            return Err(MemoryError::InvalidParameter);
        }

        let page_count = pages_for_size(size_bytes)?;
        let mut allocation_start = 0;
        self.AllocatePages(
            EFI_ALLOCATE_TYPE::AllocateAnyPages,
            memory_type,
            page_count,
            &mut allocation_start,
        )?;
        Ok(allocation_start)
    }

    /// Allocates enough 4 KiB pages to cover `size_bytes` at an aligned
    /// address chosen from the requested search direction.
    ///
    /// # Parameters
    ///
    /// - `memory_type`: EFI memory type assigned to the allocated range.
    /// - `size_bytes`: Number of bytes the allocation must cover.
    /// - `alignment`: Required physical alignment in bytes. This must be a
    ///   non-zero multiple of 4 KiB.
    /// - `direction`: Whether to search from the low or high end of RAM.
    pub fn allocate_aligned_pages_for_size(
        &mut self,
        memory_type: EFI_MEMORY_TYPE,
        size_bytes: UINTN,
        alignment: UINT64,
        direction: AllocationDirection,
    ) -> Result<EFI_PHYSICAL_ADDRESS, MemoryError> {
        if memory_type == EFI_MEMORY_TYPE::EfiMaxMemoryType {
            return Err(MemoryError::InvalidParameter);
        }

        let page_count = pages_for_size(size_bytes)?;
        self.allocate_aligned_pages(memory_type, page_count, alignment, direction)
    }

    /// Allocates pages at an address aligned to a caller-selected boundary.
    ///
    /// # Parameters
    ///
    /// - `memory_type`: EFI memory type assigned to the allocated range.
    /// - `pages`: Number of 4 KiB pages to allocate.
    /// - `alignment`: Required physical alignment in bytes. This must be a
    ///   non-zero multiple of 4 KiB.
    /// - `direction`: Whether to search from the low or high end of RAM.
    pub fn allocate_aligned_pages(
        &mut self,
        memory_type: EFI_MEMORY_TYPE,
        pages: UINTN,
        alignment: UINT64,
        direction: AllocationDirection,
    ) -> Result<EFI_PHYSICAL_ADDRESS, MemoryError> {
        if pages == 0
            || memory_type == EFI_MEMORY_TYPE::EfiMaxMemoryType
            || alignment == 0
            || !is_page_aligned(alignment)
        {
            return Err(MemoryError::InvalidParameter);
        }

        let page_count = pages_to_u64(pages)?;
        let allocation_start =
            self.find_aligned_allocation(page_count, alignment, direction)?;

        self.allocate_exact_range(allocation_start, page_count, memory_type)?;
        Ok(allocation_start)
    }

    /// Frees pages using EFI `FreePages` parameter names and types.
    ///
    /// # Parameters
    ///
    /// - `Memory`: Physical base address of the allocation to free.
    /// - `Pages`: Number of 4 KiB pages to free.
    pub fn FreePages(
        &mut self,
        Memory: EFI_PHYSICAL_ADDRESS,
        Pages: UINTN,
    ) -> Result<(), MemoryError> {
        if Pages == 0 || !is_page_aligned(Memory) {
            return Err(MemoryError::InvalidParameter);
        }

        let page_count = pages_to_u64(Pages)?;
        self.free_exact_range(Memory, page_count)
    }

    /// Adds one RAM region as EFI conventional memory.
    ///
    /// # Parameters
    ///
    /// - `region`: RAM region decoded from the device tree.
    fn add_memory_region(&mut self, region: MemoryRegion) -> Result<(), MemoryError> {
        let Some((start, end)) = align_region_to_pages(region, false)? else {
            return Ok(());
        };

        self.insert_sorted(EFI_MEMORY_DESCRIPTOR {
            Type: EFI_MEMORY_TYPE::EfiConventionalMemory as UINT32,
            PhysicalStart: start,
            VirtualStart: 0,
            NumberOfPages: bytes_to_pages(end - start),
            Attribute: 0,
        })?;

        Ok(())
    }

    /// Carves one reserved device-tree region from conventional memory.
    ///
    /// # Parameters
    ///
    /// - `region`: Reserved region decoded from the device tree.
    fn add_reserved_region(&mut self, region: MemoryRegion) -> Result<(), MemoryError> {
        let Some((reservation_start, reservation_end)) = align_region_to_pages(region, true)? else {
            return Ok(());
        };

        self.carve_range(
            reservation_start,
            reservation_end,
            EFI_MEMORY_TYPE::EfiReservedMemoryType,
        )
    }

    /// Carves one linker-defined firmware range into the page map.
    ///
    /// # Parameters
    ///
    /// - `region`: Linker-defined firmware range to classify.
    /// - `memory_type`: EFI memory type assigned to the carved range.
    fn add_firmware_region(
        &mut self,
        region: Option<(EFI_PHYSICAL_ADDRESS, EFI_PHYSICAL_ADDRESS)>,
        memory_type: EFI_MEMORY_TYPE,
    ) -> Result<(), MemoryError> {
        let Some((start, end)) = region else {
            return Ok(());
        };

        self.carve_range(start, end, memory_type)
    }

    /// Replaces overlapping conventional memory with `memory_type`.
    ///
    /// # Parameters
    ///
    /// - `start`: Inclusive physical start address of the carved range.
    /// - `end`: Exclusive physical end address of the carved range.
    /// - `memory_type`: EFI memory type assigned to the carved range.
    fn carve_range(
        &mut self,
        start: EFI_PHYSICAL_ADDRESS,
        end: EFI_PHYSICAL_ADDRESS,
        memory_type: EFI_MEMORY_TYPE,
    ) -> Result<(), MemoryError> {
        let mut index = 0usize;
        while index < self.descriptor_count {
            let descriptor = self.descriptors[index];
            let descriptor_end = descriptor_end(descriptor)?;

            if descriptor_end <= start {
                index += 1;
                continue;
            }

            if descriptor.PhysicalStart >= end {
                break;
            }

            if descriptor.Type != EFI_MEMORY_TYPE::EfiConventionalMemory as UINT32 {
                index += 1;
                continue;
            }

            let overlap_start = max(descriptor.PhysicalStart, start);
            let overlap_end = min(descriptor_end, end);
            let overlap_pages = bytes_to_pages(overlap_end - overlap_start);

            index = self.replace_range(index, overlap_start, overlap_pages, memory_type)?;
        }

        Ok(())
    }

    /// Finds the highest conventional-memory range that can satisfy `Pages`.
    ///
    /// # Parameters
    ///
    /// - `Pages`: Number of pages requested.
    fn find_allocation_any_pages(&self, Pages: UINT64) -> Result<EFI_PHYSICAL_ADDRESS, MemoryError> {
        let byte_count = pages_to_bytes(Pages as UINTN)?;
        let mut candidate = None;

        for descriptor in self.descriptors() {
            if descriptor.Type != EFI_MEMORY_TYPE::EfiConventionalMemory as UINT32 {
                continue;
            }

            let descriptor_end = descriptor_end(*descriptor)?;
            if descriptor_end < descriptor.PhysicalStart.saturating_add(byte_count) {
                continue;
            }

            let start = align_down(descriptor_end - byte_count, EFI_PAGE_SIZE);
            if start < descriptor.PhysicalStart {
                continue;
            }

            candidate = Some(candidate.map_or(start, |current| max(current, start)));
        }

        candidate.ok_or(MemoryError::OutOfResources)
    }

    /// Finds one aligned conventional-memory range for `pages`.
    ///
    /// # Parameters
    ///
    /// - `pages`: Number of pages requested.
    /// - `alignment`: Required allocation alignment in bytes.
    /// - `direction`: Whether to search from the low or high end of RAM.
    fn find_aligned_allocation(
        &self,
        pages: UINT64,
        alignment: UINT64,
        direction: AllocationDirection,
    ) -> Result<EFI_PHYSICAL_ADDRESS, MemoryError> {
        let byte_count = pages_to_bytes(pages as UINTN)?;
        let mut candidate = None;

        for descriptor in self.descriptors() {
            if descriptor.Type != EFI_MEMORY_TYPE::EfiConventionalMemory as UINT32 {
                continue;
            }

            let descriptor_end = descriptor_end(*descriptor)?;
            let Some(latest_start) = descriptor_end.checked_sub(byte_count) else {
                continue;
            };
            if latest_start < descriptor.PhysicalStart {
                continue;
            }

            let start = match direction {
                AllocationDirection::Low =>
                    align_up(descriptor.PhysicalStart, alignment)?,
                AllocationDirection::High =>
                    align_down(latest_start, alignment),
            };

            let Some(end) = start.checked_add(byte_count) else {
                continue;
            };
            if start < descriptor.PhysicalStart || end > descriptor_end {
                continue;
            }

            candidate = Some(match (candidate, direction) {
                (Some(current), AllocationDirection::Low) => min(current, start),
                (Some(current), AllocationDirection::High) => max(current, start),
                (None, _) => start,
            });
        }

        candidate.ok_or(MemoryError::OutOfResources)
    }

    /// Finds the highest conventional-memory range below `Memory` that fits `Pages` bytes.
    ///
    /// # Parameters
    ///
    /// - `Pages`: Requested size in bytes.
    /// - `Memory`: Maximum acceptable address supplied by the caller.
    fn find_allocation_max_address(
        &self,
        Pages: UINT64,
        Memory: EFI_PHYSICAL_ADDRESS,
    ) -> Result<EFI_PHYSICAL_ADDRESS, MemoryError> {
        let mut candidate = None;

        for descriptor in self.descriptors() {
            if descriptor.Type != EFI_MEMORY_TYPE::EfiConventionalMemory as UINT32 {
                continue;
            }

            let descriptor_end = descriptor_end(*descriptor)?;
            let upper_bound = min(descriptor_end, Memory.saturating_add(1));
            if upper_bound < descriptor.PhysicalStart.saturating_add(Pages) {
                continue;
            }

            let start = align_down(upper_bound - Pages, EFI_PAGE_SIZE);
            if start < descriptor.PhysicalStart {
                continue;
            }

            candidate = Some(candidate.map_or(start, |current| max(current, start)));
        }

        candidate.ok_or(MemoryError::OutOfResources)
    }

    /// Converts an exact conventional-memory range into `MemoryType`.
    ///
    /// # Parameters
    ///
    /// - `Memory`: Physical base address of the range.
    /// - `Pages`: Number of pages in the range.
    /// - `MemoryType`: EFI memory type assigned to the range.
    fn allocate_exact_range(
        &mut self,
        Memory: EFI_PHYSICAL_ADDRESS,
        Pages: UINT64,
        MemoryType: EFI_MEMORY_TYPE,
    ) -> Result<(), MemoryError> {
        let range_end = checked_end(Memory, Pages)?;
        let index = self.find_descriptor_covering(Memory, range_end)?;
        if self.descriptors[index].Type != EFI_MEMORY_TYPE::EfiConventionalMemory as UINT32 {
            return Err(MemoryError::OutOfResources);
        }

        self.replace_range(index, Memory, Pages, MemoryType)?;
        self.coalesce();
        Ok(())
    }

    /// Converts an allocated range back into conventional memory.
    ///
    /// # Parameters
    ///
    /// - `Memory`: Physical base address of the range.
    /// - `Pages`: Number of pages in the range.
    fn free_exact_range(
        &mut self,
        Memory: EFI_PHYSICAL_ADDRESS,
        Pages: UINT64,
    ) -> Result<(), MemoryError> {
        let range_end = checked_end(Memory, Pages)?;
        let index = self.find_descriptor_covering(Memory, range_end)?;
        let descriptor_type = self.descriptors[index].Type;
        if descriptor_type == EFI_MEMORY_TYPE::EfiConventionalMemory as UINT32
            || descriptor_type == EFI_MEMORY_TYPE::EfiReservedMemoryType as UINT32
        {
            return Err(MemoryError::InvalidParameter);
        }

        self.replace_range(index, Memory, Pages, EFI_MEMORY_TYPE::EfiConventionalMemory)?;
        self.coalesce();
        Ok(())
    }

    /// Finds the descriptor that fully contains the half-open range `[start, end)`.
    ///
    /// # Parameters
    ///
    /// - `start`: Physical start address of the target range.
    /// - `end`: Physical end address of the target range.
    fn find_descriptor_covering(
        &self,
        start: EFI_PHYSICAL_ADDRESS,
        end: EFI_PHYSICAL_ADDRESS,
    ) -> Result<usize, MemoryError> {
        for (index, descriptor) in self.descriptors().iter().copied().enumerate() {
            let descriptor_end = descriptor_end(descriptor)?;
            if descriptor.PhysicalStart <= start && descriptor_end >= end {
                return Ok(index);
            }
        }

        Err(MemoryError::NotFound)
    }

    /// Replaces a subrange of one descriptor with a different EFI memory type.
    ///
    /// # Parameters
    ///
    /// - `index`: Descriptor index to modify.
    /// - `Memory`: Physical start address of the replacement range.
    /// - `Pages`: Number of pages in the replacement range.
    /// - `MemoryType`: EFI memory type assigned to the replacement range.
    fn replace_range(
        &mut self,
        index: usize,
        Memory: EFI_PHYSICAL_ADDRESS,
        Pages: UINT64,
        MemoryType: EFI_MEMORY_TYPE,
    ) -> Result<usize, MemoryError> {
        let descriptor = self.descriptors[index];
        let descriptor_end = descriptor_end(descriptor)?;
        let range_end = checked_end(Memory, Pages)?;
        if Memory < descriptor.PhysicalStart || range_end > descriptor_end {
            return Err(MemoryError::InvalidParameter);
        }

        let prefix_pages = bytes_to_pages(Memory - descriptor.PhysicalStart);
        let suffix_pages = bytes_to_pages(descriptor_end - range_end);
        let mut parts = [EMPTY_DESCRIPTOR; 3];
        let mut part_count = 0usize;
        let mut next_index = index + 1;

        if prefix_pages != 0 {
            parts[part_count] = EFI_MEMORY_DESCRIPTOR {
                Type: descriptor.Type,
                PhysicalStart: descriptor.PhysicalStart,
                VirtualStart: descriptor.VirtualStart,
                NumberOfPages: prefix_pages,
                Attribute: descriptor.Attribute,
            };
            part_count += 1;
            next_index += 1;
        }

        parts[part_count] = EFI_MEMORY_DESCRIPTOR {
            Type: MemoryType as UINT32,
            PhysicalStart: Memory,
            VirtualStart: 0,
            NumberOfPages: Pages,
            Attribute: descriptor.Attribute,
        };
        part_count += 1;

        if suffix_pages != 0 {
            parts[part_count] = EFI_MEMORY_DESCRIPTOR {
                Type: descriptor.Type,
                PhysicalStart: range_end,
                VirtualStart: 0,
                NumberOfPages: suffix_pages,
                Attribute: descriptor.Attribute,
            };
            part_count += 1;
        }

        self.replace_descriptor(index, &parts[..part_count])?;
        Ok(next_index)
    }

    /// Inserts one descriptor while keeping the map sorted by physical address.
    ///
    /// # Parameters
    ///
    /// - `descriptor`: Descriptor to insert.
    fn insert_sorted(&mut self, descriptor: EFI_MEMORY_DESCRIPTOR) -> Result<(), MemoryError> {
        let mut index = 0usize;
        while index < self.descriptor_count && self.descriptors[index].PhysicalStart <= descriptor.PhysicalStart {
            index += 1;
        }

        self.insert_descriptor(index, descriptor)?;
        Ok(())
    }

    /// Inserts one descriptor at `index`.
    ///
    /// # Parameters
    ///
    /// - `index`: Insertion point in the descriptor buffer.
    /// - `descriptor`: Descriptor to insert.
    fn insert_descriptor(
        &mut self,
        index: usize,
        descriptor: EFI_MEMORY_DESCRIPTOR,
    ) -> Result<(), MemoryError> {
        if self.descriptor_count == self.descriptors.len() || index > self.descriptor_count {
            return Err(MemoryError::BufferTooSmall);
        }

        let mut cursor = self.descriptor_count;
        while cursor > index {
            self.descriptors[cursor] = self.descriptors[cursor - 1];
            cursor -= 1;
        }

        self.descriptors[index] = descriptor;
        self.descriptor_count += 1;
        Ok(())
    }

    /// Replaces one descriptor with a short list of new descriptors.
    ///
    /// # Parameters
    ///
    /// - `index`: Descriptor index to replace.
    /// - `replacements`: Replacement descriptors written in order.
    fn replace_descriptor(
        &mut self,
        index: usize,
        replacements: &[EFI_MEMORY_DESCRIPTOR],
    ) -> Result<(), MemoryError> {
        if index >= self.descriptor_count || replacements.is_empty() {
            return Err(MemoryError::InvalidParameter);
        }

        let new_count = self.descriptor_count - 1 + replacements.len();
        if new_count > self.descriptors.len() {
            return Err(MemoryError::BufferTooSmall);
        }

        if replacements.len() > 1 {
            let growth = replacements.len() - 1;
            let mut cursor = self.descriptor_count;
            while cursor > index + 1 {
                self.descriptors[cursor + growth - 1] = self.descriptors[cursor - 1];
                cursor -= 1;
            }
        }

        for (offset, replacement) in replacements.iter().copied().enumerate() {
            self.descriptors[index + offset] = replacement;
        }

        self.descriptor_count = new_count;
        Ok(())
    }

    /// Merges adjacent compatible descriptors after insertions and splits.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn coalesce(&mut self) {
        let mut index = 0usize;
        while index + 1 < self.descriptor_count {
            let current = self.descriptors[index];
            let next = self.descriptors[index + 1];

            if descriptors_are_compatible(current, next) {
                let next_end = descriptor_end(next).unwrap_or(next.PhysicalStart);
                self.descriptors[index].NumberOfPages = bytes_to_pages(next_end - current.PhysicalStart);

                let mut cursor = index + 2;
                while cursor < self.descriptor_count {
                    self.descriptors[cursor - 1] = self.descriptors[cursor];
                    cursor += 1;
                }

                self.descriptor_count -= 1;
            } else {
                index += 1;
            }
        }
    }
}

/// Builds the active EFI-style memory map from an FDT.
///
/// # Parameters
///
/// - `fdt`: Flattened device tree supplying RAM and reservation ranges.
/// - `memory_regions`: Scratch slice that receives `/memory` ranges.
/// - `reserved_regions`: Scratch slice that receives reservation ranges.
/// - `descriptors`: Descriptor buffer used for the EFI-style memory map.
pub fn memory_map_from_fdt(
    fdt: &Fdt,
    memory_regions: &mut [MemoryRegion],
    reserved_regions: &mut [MemoryRegion],
    descriptors: &mut [EFI_MEMORY_DESCRIPTOR],
) -> Result<usize, MemoryError> {
    let allocator = PageAllocator::from_fdt(fdt, memory_regions, reserved_regions, descriptors)?;
    Ok(allocator.descriptor_count())
}

/// Empty descriptor value used for temporary fixed-size arrays.
const EMPTY_DESCRIPTOR: EFI_MEMORY_DESCRIPTOR = EFI_MEMORY_DESCRIPTOR {
    Type: EFI_MEMORY_TYPE::EfiReservedMemoryType as UINT32,
    PhysicalStart: 0,
    VirtualStart: 0,
    NumberOfPages: 0,
    Attribute: 0,
};

/// Aligns a raw device-tree region to page boundaries.
///
/// # Parameters
///
/// - `region`: Device-tree region to align.
/// - `reserve_partial_pages`: Whether partial edge pages must be reserved.
fn align_region_to_pages(
    region: MemoryRegion,
    reserve_partial_pages: bool,
) -> Result<Option<(EFI_PHYSICAL_ADDRESS, EFI_PHYSICAL_ADDRESS)>, MemoryError> {
    let end = region.base.checked_add(region.size).ok_or(MemoryError::AddressOverflow)?;
    let start = if reserve_partial_pages {
        align_down(region.base, EFI_PAGE_SIZE)
    } else {
        align_up(region.base, EFI_PAGE_SIZE)?
    };
    let end = if reserve_partial_pages {
        align_up(end, EFI_PAGE_SIZE)?
    } else {
        align_down(end, EFI_PAGE_SIZE)
    };

    if end <= start {
        return Ok(None);
    }

    Ok(Some((start, end)))
}

/// Converts linker-defined start and end addresses into a page-aligned range.
///
/// # Parameters
///
/// - `start`: Start address supplied by the linker.
/// - `end`: End address supplied by the linker.
fn linker_region(
    start: EFI_PHYSICAL_ADDRESS,
    end: EFI_PHYSICAL_ADDRESS,
) -> Result<Option<(EFI_PHYSICAL_ADDRESS, EFI_PHYSICAL_ADDRESS)>, MemoryError> {
    if end <= start {
        return Ok(None);
    }

    align_region_to_pages(
        MemoryRegion {
            base: start,
            size: end - start,
        },
        true,
    )
}

/// Returns the runtime address of the linker-defined firmware code start.
fn firmware_code_start() -> EFI_PHYSICAL_ADDRESS {
    let address: usize;

    unsafe {
        asm!(
            "lla {address}, __firmware_code_start",
            address = lateout(reg) address,
            options(nomem, nostack, preserves_flags)
        );
    }

    address as EFI_PHYSICAL_ADDRESS
}

/// Returns the runtime address of the linker-defined firmware code end.
fn firmware_code_end() -> EFI_PHYSICAL_ADDRESS {
    let address: usize;

    unsafe {
        asm!(
            "lla {address}, __firmware_code_end",
            address = lateout(reg) address,
            options(nomem, nostack, preserves_flags)
        );
    }

    address as EFI_PHYSICAL_ADDRESS
}

/// Returns the runtime address of the linker-defined firmware data start.
fn firmware_data_start() -> EFI_PHYSICAL_ADDRESS {
    let address: usize;

    unsafe {
        asm!(
            "lla {address}, __firmware_data_start",
            address = lateout(reg) address,
            options(nomem, nostack, preserves_flags)
        );
    }

    address as EFI_PHYSICAL_ADDRESS
}

/// Returns the runtime address of the linker-defined firmware data end.
fn firmware_data_end() -> EFI_PHYSICAL_ADDRESS {
    let address: usize;

    unsafe {
        asm!(
            "lla {address}, __firmware_data_end",
            address = lateout(reg) address,
            options(nomem, nostack, preserves_flags)
        );
    }

    address as EFI_PHYSICAL_ADDRESS
}

/// Returns the runtime address of the linker-defined firmware heap start.
fn firmware_heap_start() -> EFI_PHYSICAL_ADDRESS {
    let address: usize;

    unsafe {
        asm!(
            "lla {address}, __heap_start",
            address = lateout(reg) address,
            options(nomem, nostack, preserves_flags)
        );
    }

    address as EFI_PHYSICAL_ADDRESS
}

/// Returns the runtime address of the linker-defined firmware heap end.
fn firmware_heap_end() -> EFI_PHYSICAL_ADDRESS {
    let address: usize;

    unsafe {
        asm!(
            "lla {address}, __heap_end",
            address = lateout(reg) address,
            options(nomem, nostack, preserves_flags)
        );
    }

    address as EFI_PHYSICAL_ADDRESS
}

/// Returns the runtime address of the linker-defined firmware stack bottom.
fn firmware_stack_bottom() -> EFI_PHYSICAL_ADDRESS {
    let address: usize;

    unsafe {
        asm!(
            "lla {address}, __stack_bottom",
            address = lateout(reg) address,
            options(nomem, nostack, preserves_flags)
        );
    }

    address as EFI_PHYSICAL_ADDRESS
}

/// Returns the runtime address of the linker-defined firmware stack top.
fn firmware_stack_top() -> EFI_PHYSICAL_ADDRESS {
    let address: usize;

    unsafe {
        asm!(
            "lla {address}, __stack_top",
            address = lateout(reg) address,
            options(nomem, nostack, preserves_flags)
        );
    }

    address as EFI_PHYSICAL_ADDRESS
}

/// Converts a page count from `UINTN` to `UINT64`.
///
/// # Parameters
///
/// - `pages`: Page count supplied by an EFI-style caller.
fn pages_to_u64(pages: UINTN) -> Result<UINT64, MemoryError> {
    UINT64::try_from(pages).map_err(|_| MemoryError::InvalidParameter)
}

/// Converts a page count into bytes.
///
/// # Parameters
///
/// - `pages`: Page count supplied by an EFI-style caller.
fn pages_to_bytes(pages: UINTN) -> Result<UINT64, MemoryError> {
    pages_to_u64(pages)?
        .checked_mul(EFI_PAGE_SIZE)
        .ok_or(MemoryError::AddressOverflow)
}

/// Converts a byte count into the minimum number of 4 KiB pages needed.
///
/// # Parameters
///
/// - `size_bytes`: Number of bytes the allocation must cover.
fn pages_for_size(size_bytes: UINTN) -> Result<UINTN, MemoryError> {
    let size = pages_to_u64(size_bytes)?;
    let rounded = align_up(max(size, 1), EFI_PAGE_SIZE)?;
    usize::try_from(bytes_to_pages(rounded))
        .map_err(|_| MemoryError::InvalidParameter)
}

/// Converts a byte count that is already page aligned into pages.
///
/// # Parameters
///
/// - `bytes`: Page-aligned byte count.
fn bytes_to_pages(bytes: UINT64) -> UINT64 {
    bytes / EFI_PAGE_SIZE
}

/// Returns `true` when `address` is 4 KiB aligned.
///
/// # Parameters
///
/// - `address`: Address to test.
fn is_page_aligned(address: EFI_PHYSICAL_ADDRESS) -> bool {
    (address & (EFI_PAGE_SIZE - 1)) == 0
}

/// Aligns `value` down to `alignment`.
///
/// # Parameters
///
/// - `value`: Value to align.
/// - `alignment`: Power-of-two alignment in bytes.
fn align_down(value: UINT64, alignment: UINT64) -> UINT64 {
    value & !(alignment - 1)
}

/// Aligns `value` up to `alignment`.
///
/// # Parameters
///
/// - `value`: Value to align.
/// - `alignment`: Power-of-two alignment in bytes.
fn align_up(value: UINT64, alignment: UINT64) -> Result<UINT64, MemoryError> {
    let mask = alignment - 1;
    value
        .checked_add(mask)
        .map(|aligned| aligned & !mask)
        .ok_or(MemoryError::AddressOverflow)
}

/// Computes the exclusive end address of a descriptor.
///
/// # Parameters
///
/// - `descriptor`: Descriptor whose end address is requested.
fn descriptor_end(descriptor: EFI_MEMORY_DESCRIPTOR) -> Result<EFI_PHYSICAL_ADDRESS, MemoryError> {
    checked_end(descriptor.PhysicalStart, descriptor.NumberOfPages)
}

/// Computes the exclusive end address of a page range.
///
/// # Parameters
///
/// - `start`: Physical base address of the range.
/// - `pages`: Number of pages in the range.
fn checked_end(start: EFI_PHYSICAL_ADDRESS, pages: UINT64) -> Result<EFI_PHYSICAL_ADDRESS, MemoryError> {
    start
        .checked_add(pages.checked_mul(EFI_PAGE_SIZE).ok_or(MemoryError::AddressOverflow)?)
        .ok_or(MemoryError::AddressOverflow)
}

/// Returns `true` when two descriptors can be merged into one.
///
/// # Parameters
///
/// - `left`: Earlier descriptor in address order.
/// - `right`: Later descriptor in address order.
fn descriptors_are_compatible(left: EFI_MEMORY_DESCRIPTOR, right: EFI_MEMORY_DESCRIPTOR) -> bool {
    if left.Type != right.Type || left.Attribute != right.Attribute || left.VirtualStart != right.VirtualStart {
        return false;
    }

    match descriptor_end(left) {
        Ok(left_end) => right.PhysicalStart <= left_end,
        Err(_) => false,
    }
}