#![no_std]
#![no_main]

//! Freestanding RISC-V firmware entry point.
//!
//! The binary boots under OpenSBI, captures the boot hart and incoming device
//! tree pointer, initializes a fixed stack at the firmware load address,
//! probes GPT partitions on VirtIO block devices, and boots Linux from the
//! first boot-flagged partition that provides a supported filesystem plus the
//! required kernel artifacts.

/// VirtIO MMIO transport, queue, and block-device support.
pub mod virtio;
/// GUID partition table parsing for block devices.
pub mod gpt;
/// Generic partition-table interfaces for GPT and future MBR support.
pub mod partition;
/// Filesystem abstractions shared by FAT and future formats.
pub mod filesystem;
/// Linux boot-method request construction.
pub mod linux;
/// Read-only FAT filesystem support for loading files by path.
pub mod fat;
/// Read-only ext4 filesystem support for loading files by path.
pub mod ext4;
/// Boot-oriented device-tree object stub.
pub mod dtb;
/// Flattened device tree parsing helpers used for diagnostics and future edits.
pub mod devicetree;
/// Firmware diagnostics output and reporting.
pub mod diagnostics;
/// Formatted console output helpers built on top of OpenSBI.
pub mod print;
/// OpenSBI constants and environment-call wrappers.
pub mod sbi;
/// EFI-style page allocator and memory map support.
pub mod memory;

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
use core::ptr;
use core::str;
use devicetree::{Fdt, MemoryRegion};
use diagnostics::print_diagnostics;
use dtb::Dtb;
use ext4::Ext4Volume;
use fat::FatVolume;
use filesystem::{FileHandle, FileInfo, FileSystem, FileType, LoadedFile};
use gpt::GptPartitionTable;
use linux::{boot as linux_boot, check_kernel_header, start as linux_start};
use memory::{EFI_ALLOCATE_TYPE, EFI_MEMORY_DESCRIPTOR, EFI_MEMORY_TYPE, PageAllocator};
use partition::{PartitionEntry, PartitionTable};
use sbi::{poweroff, poweroff_on_failure};
use virtio::{qemu_virt_block_devices, BlockDevice};
use virtio::VirtioBlockDriver;

unsafe extern "C" {
    /// Linker-defined top of the firmware-owned runtime stack.
    static __stack_top: u8;
    /// Linker-defined start of the firmware runtime image.
    static __firmware_code_start: u8;
    /// Assembly relocation entry point inside the firmware image.
    static relocated_entry: u8;
    /// Linker-defined end of the reserved firmware runtime image window.
    static __heap_end: u8;
}

/// Build profile name injected by the Makefile for runtime diagnostics.
const BUILD_PROFILE: &str = match option_env!("PROFILE_NAME") {
    Some(profile) => profile,
    None => "unknown",
};
/// OpenSBI loads the primary firmware image at this physical address on QEMU virt.
const PRIMARY_FIRMWARE_LOAD_ADDRESS: usize = 0x8020_0000;
/// Linux is loaded at the conventional physical start address on QEMU virt.
const KERNEL_LOAD_ADDRESS: usize = 0x8020_0000;

global_asm!(
    r#"
    .section .text.entry
    .globl _start
_start:
    mv a2, sp
    lla sp, __stack_top
    tail rust_entry

    .globl relocated_entry
relocated_entry:
    mv a2, sp
    lla sp, __stack_top
    tail rust_relocated_entry
"#
);

#[unsafe(no_mangle)]
/// Boot hart identifier captured from register `a0` at firmware entry.
static mut BOOT_HART_ID: usize = 0;

#[unsafe(no_mangle)]
/// Flattened device-tree pointer captured from register `a1` at firmware entry.
static mut DEVICE_TREE_PTR: usize = 0;

#[unsafe(no_mangle)]
/// Stack pointer observed on entry before switching to the firmware stack.
static mut ENTRY_STACK_PTR: usize = 0;

/// Returns the hart identifier observed at firmware entry.
///
/// This function reads the hart identifier from a volatile memory location.
pub fn boot_hart_id() -> usize {
    unsafe { ptr::read_volatile(ptr::addr_of!(BOOT_HART_ID)) }
}

/// Returns the pointer to the flattened device tree passed in register `a1`.
pub fn device_tree_ptr() -> *const u8 {
    unsafe { ptr::read_volatile(ptr::addr_of!(DEVICE_TREE_PTR)) as *const u8 }
}

/// Returns the stack pointer value observed at firmware entry, before switching
/// to the firmware-owned stack.
pub fn entry_stack_ptr() -> usize {
    unsafe { ptr::read_volatile(ptr::addr_of!(ENTRY_STACK_PTR)) }
}

/// Returns the top address of the linker-defined firmware-owned stack.
pub fn stack_top() -> usize {
    PRIMARY_FIRMWARE_LOAD_ADDRESS
}

/// Returns the runtime base address of the current firmware image.
fn firmware_runtime_base() -> usize {
    let value: usize;

    unsafe {
        asm!(
            "lla {value}, __firmware_code_start",
            value = lateout(reg) value,
            options(nomem, nostack, preserves_flags)
        );
    }

    value
}

/// Returns the runtime size in bytes of the reserved firmware image window.
fn firmware_runtime_size() -> usize {
    core::ptr::addr_of!(__heap_end) as usize
}

/// Returns the linked offset of the relocated firmware entry inside the image.
fn relocated_entry_offset() -> usize {
    core::ptr::addr_of!(relocated_entry) as usize
}

/// Prints the firmware name, version, and build profile.
///
/// # Parameters
///
/// This function does not accept parameters.
fn greet() {
    crate::println!(
        "{} {} ({})",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        BUILD_PROFILE,
    );
}

/// Candidate kernel paths searched in order on boot-flagged partitions.
const KERNEL_CANDIDATE_PATHS: [&str; 2] = ["/boot/vmlinuz", "/vmlinuz"];
/// Candidate initrd paths searched in order on boot-flagged partitions.
const INITRD_CANDIDATE_PATHS: [&str; 2] = ["/boot/initrd.img", "/initrd.img"];

/// Prints one loaded file path plus size with a filesystem prefix.
///
/// # Parameters
///
/// - `prefix`: Filesystem label shown before the file path.
/// - `path`: Absolute path of the loaded file.
/// - `loaded_file`: Loaded file metadata including physical address and size.
fn print_loaded_file(prefix: &str, path: &str, loaded_file: &LoadedFile) {
    crate::println!(
        "{}: loaded '{}', size={} @ {:#018x}",
        prefix,
        path,
        loaded_file.size_bytes(),
        loaded_file.physical_start() as usize,
    );
}

/// Tries the Linux boot method on one boot-flagged partition.
///
/// # Parameters
///
/// - `device`: Block device that contains the partition.
/// - `partition`: Partition entry chosen for Linux boot.
/// - `filesystem`: Filesystem classification derived from probing the partition start.
/// - `block_device_index`: Zero-based virtio block-device index.
/// - `partition_number`: One-based partition number within the GPT.
/// - `boot_hart`: Original hart identifier received from OpenSBI.
/// - `device_tree_ptr`: Boot-time device-tree pointer received from OpenSBI.
fn try_linux_boot_from_partition<D: BlockDevice, P: PartitionEntry>(
    device: &mut D,
    partition: P,
    filesystem: DetectedFilesystem,
    block_device_index: usize,
    partition_number: u32,
    boot_hart: usize,
    device_tree_ptr: *const u8,
) {
    match filesystem {
        DetectedFilesystem::Fat => {
            let mut volume = match FatVolume::new(device, partition.first_lba()) {
                Ok(volume) => volume,
                Err(_) => return,
            };
            try_linux_boot_from_fat_volume(
                &mut volume,
                "fat",
                block_device_index,
                partition_number,
                boot_hart,
                device_tree_ptr,
            );
        }
        DetectedFilesystem::Ext4 => {
            let mut volume = match Ext4Volume::new(device, partition.first_lba()) {
                Ok(volume) => volume,
                Err(_) => return,
            };
            try_linux_boot_from_ext4_volume(
                &mut volume,
                "ext4",
                block_device_index,
                partition_number,
                boot_hart,
                device_tree_ptr,
            );
        }
        DetectedFilesystem::Unknown => {}
    }
}

/// Tries the Linux boot method using one FAT filesystem on a boot-flagged partition.
///
/// # Parameters
///
/// - `volume`: Mounted FAT filesystem chosen from the boot-flagged partition.
/// - `filesystem_name`: Filesystem label used in loaded-file logs.
/// - `block_device_index`: Zero-based virtio block-device index.
/// - `partition_number`: One-based partition number within the GPT.
/// - `boot_hart`: Original hart identifier received from OpenSBI.
/// - `device_tree_ptr`: Boot-time device-tree pointer received from OpenSBI.
fn try_linux_boot_from_fat_volume<D: BlockDevice>(
    volume: &mut FatVolume<'_, D>,
    filesystem_name: &str,
    block_device_index: usize,
    partition_number: u32,
    boot_hart: usize,
    device_tree_ptr: *const u8,
) {
    let mut regions = [MemoryRegion { base: 0, size: 0 }; 8];
    let mut reserved = [MemoryRegion { base: 0, size: 0 }; 16];
    let mut memory_map = [EMPTY_MEMORY_DESCRIPTOR; 32];
    let mut allocator = match page_allocator_from_live_fdt(
        device_tree_ptr,
        &mut regions,
        &mut reserved,
        &mut memory_map,
    ) {
        Some(allocator) => allocator,
        None => {
            crate::println!("linux: page allocator unavailable");
            return;
        }
    };

    let Some((kernel_path, kernel_loaded)) = load_first_fat_file_at(
        volume,
        &KERNEL_CANDIDATE_PATHS,
        &mut allocator,
        KERNEL_LOAD_ADDRESS as u64,
    ) else {
        return;
    };
    let initrd_loaded = load_first_fat_file(
        volume,
        &INITRD_CANDIDATE_PATHS,
        &mut allocator,
    );

    // Reuse the same allocator state for artifact loads and DTB cloning so
    // later allocations cannot overlap the already-loaded kernel or initrd.
    boot_loaded_linux_artifacts(
        filesystem_name,
        kernel_path,
        &kernel_loaded,
        initrd_loaded.as_ref().map(|(path, file)| (*path, file)),
        &mut allocator,
        block_device_index,
        partition_number,
        boot_hart,
        device_tree_ptr,
    );
}

/// Tries the Linux boot method using one ext4 filesystem on a boot-flagged partition.
///
/// # Parameters
///
/// - `volume`: Mounted ext4 filesystem chosen from the boot-flagged partition.
/// - `filesystem_name`: Filesystem label used in loaded-file logs.
/// - `block_device_index`: Zero-based virtio block-device index.
/// - `partition_number`: One-based partition number within the GPT.
/// - `boot_hart`: Original hart identifier received from OpenSBI.
/// - `device_tree_ptr`: Boot-time device-tree pointer received from OpenSBI.
fn try_linux_boot_from_ext4_volume<D: BlockDevice>(
    volume: &mut Ext4Volume<'_, D>,
    filesystem_name: &str,
    block_device_index: usize,
    partition_number: u32,
    boot_hart: usize,
    device_tree_ptr: *const u8,
) {
    let mut regions = [MemoryRegion { base: 0, size: 0 }; 8];
    let mut reserved = [MemoryRegion { base: 0, size: 0 }; 16];
    let mut memory_map = [EMPTY_MEMORY_DESCRIPTOR; 32];
    let mut allocator = match page_allocator_from_live_fdt(
        device_tree_ptr,
        &mut regions,
        &mut reserved,
        &mut memory_map,
    ) {
        Some(allocator) => allocator,
        None => {
            crate::println!("linux: page allocator unavailable");
            return;
        }
    };

    let Some((kernel_path, kernel_loaded)) = load_first_ext4_file_at(
        volume,
        &KERNEL_CANDIDATE_PATHS,
        &mut allocator,
        KERNEL_LOAD_ADDRESS as u64,
    ) else {
        return;
    };
    let initrd_loaded = load_first_ext4_file(
        volume,
        &INITRD_CANDIDATE_PATHS,
        &mut allocator,
    );

    // Reuse the same allocator state for artifact loads and DTB cloning so
    // later allocations cannot overlap the already-loaded kernel or initrd.
    boot_loaded_linux_artifacts(
        filesystem_name,
        kernel_path,
        &kernel_loaded,
        initrd_loaded.as_ref().map(|(path, file)| (*path, file)),
        &mut allocator,
        block_device_index,
        partition_number,
        boot_hart,
        device_tree_ptr,
    );
}

/// Tries the Linux boot method using already loaded boot artifacts.
///
/// # Parameters
///
/// - `filesystem_name`: Filesystem label used in loaded-file logs.
/// - `kernel_path`: Absolute path of the loaded kernel image.
/// - `kernel_loaded`: Loaded kernel image placed at the Linux load address.
/// - `initrd_loaded`: Optional loaded initrd image, preserved when present.
/// - `allocator`: Live page allocator reused for DTB cloning after artifact loads.
/// - `block_device_index`: Zero-based virtio block-device index.
/// - `partition_number`: One-based partition number within the GPT.
/// - `boot_hart`: Original hart identifier received from OpenSBI.
/// - `device_tree_ptr`: Boot-time device-tree pointer received from OpenSBI.
fn boot_loaded_linux_artifacts(
    filesystem_name: &str,
    kernel_path: &str,
    kernel_loaded: &LoadedFile,
    initrd_loaded: Option<(&str, &LoadedFile)>,
    allocator: &mut PageAllocator<'_>,
    block_device_index: usize,
    partition_number: u32,
    boot_hart: usize,
    device_tree_ptr: *const u8,
) {
    let mut command_line_buffer = [0u8; 24];
    let Some(command_line) = root_command_line(
        block_device_index,
        partition_number,
        &mut command_line_buffer,
    ) else {
        crate::println!("linux: unsupported root device index");
        return;
    };

    let device_tree = match Dtb::from_ptr(device_tree_ptr) {
        Ok(device_tree) => device_tree,
        Err(_) => {
            crate::println!("linux: invalid device-tree pointer");
            return;
        }
    };
    print_loaded_file(filesystem_name, kernel_path, kernel_loaded);

    match check_kernel_header(kernel_loaded) {
        Ok(()) => {
            crate::println!(
                "linux: kernel object {} matches RISC-V boot image header",
                kernel_path,
            );
        }
        Err(_) => {
            crate::println!(
                "linux: kernel object {} does not match RISC-V boot image header",
                kernel_path,
            );
            return;
        }
    }

    if let Some((initrd_path, initrd_file)) = initrd_loaded {
        print_loaded_file(filesystem_name, initrd_path, initrd_file);
    }

    let kernel_info = FileInfo::new(FileType::File, kernel_loaded.size_bytes());
    let initrd_info = initrd_loaded.map(|(_, file)| {
        FileInfo::new(FileType::File, file.size_bytes())
    });

    match linux_boot(
        &kernel_info,
        initrd_info.as_ref(),
        &device_tree,
        allocator,
        command_line,
    ) {
        Ok(mut request) => {
            match request.update_device_tree(
                initrd_loaded.map(|(_, file)| file),
                command_line,
            ) {
                Ok(()) => {}
                Err(_) => {
                    crate::println!("linux: failed to update cloned device-tree");
                    return;
                }
            }
            crate::print!(
                "linux: boot request invoked {}={} @ {:#018x}",
                kernel_path,
                request.kernel_size_bytes(),
                kernel_loaded.physical_start() as usize,
            );
            if let Some((initrd_path, initrd_file)) = initrd_loaded {
                crate::print!(
                    ", {}={} @ {:#018x}",
                    initrd_path,
                    initrd_file.size_bytes(),
                    initrd_file.physical_start() as usize,
                );
            }
            crate::println!(", cmdline='{}'", command_line);
            crate::println!("linux: transferring control to kernel");

            unsafe {
                linux_start(
                    kernel_loaded,
                    boot_hart,
                    request.device_tree().pointer(),
                );
            }
        }
        Err(_) => {
            crate::println!("linux: boot request rejected");
        }
    }
}

/// Loads the first successfully opened file in `paths` from one FAT volume.
fn load_first_fat_file<'a, D: BlockDevice>(
    volume: &mut FatVolume<'_, D>,
    paths: &'a [&'a str],
    allocator: &mut PageAllocator<'_>,
) -> Option<(&'a str, LoadedFile)> {
    let mut index = 0usize;
    while index < paths.len() {
        let path = paths[index];
        if let Ok(mut file) = volume.open(path) {
            if let Ok(loaded) = file.load(allocator) {
                return Some((path, loaded));
            }
        }
        index += 1;
    }
    None
}

/// Loads the first successfully opened file in `paths` from one FAT volume at one fixed address.
fn load_first_fat_file_at<'a, D: BlockDevice>(
    volume: &mut FatVolume<'_, D>,
    paths: &'a [&'a str],
    allocator: &mut PageAllocator<'_>,
    physical_start: u64,
) -> Option<(&'a str, LoadedFile)> {
    let mut index = 0usize;
    while index < paths.len() {
        let path = paths[index];
        if let Ok(mut file) = volume.open(path) {
            if let Ok(loaded) = file.load_at(allocator, physical_start) {
                return Some((path, loaded));
            }
        }
        index += 1;
    }
    None
}

/// Loads the first successfully opened file in `paths` from one ext4 volume.
fn load_first_ext4_file<'a, D: BlockDevice>(
    volume: &mut Ext4Volume<'_, D>,
    paths: &'a [&'a str],
    allocator: &mut PageAllocator<'_>,
) -> Option<(&'a str, LoadedFile)> {
    let mut index = 0usize;
    while index < paths.len() {
        let path = paths[index];
        if let Ok(mut file) = volume.open(path) {
            if let Ok(loaded) = file.load(allocator) {
                return Some((path, loaded));
            }
        }
        index += 1;
    }
    None
}

/// Loads the first successfully opened file in `paths` from one ext4 volume at one fixed address.
fn load_first_ext4_file_at<'a, D: BlockDevice>(
    volume: &mut Ext4Volume<'_, D>,
    paths: &'a [&'a str],
    allocator: &mut PageAllocator<'_>,
    physical_start: u64,
) -> Option<(&'a str, LoadedFile)> {
    let mut index = 0usize;
    while index < paths.len() {
        let path = paths[index];
        if let Ok(mut file) = volume.open(path) {
            if let Ok(loaded) = file.load_at(allocator, physical_start) {
                return Some((path, loaded));
            }
        }
        index += 1;
    }
    None
}

/// Builds a page allocator from the live boot-time device tree.
///
/// # Parameters
///
/// - `memory_regions`: Scratch slice that receives `/memory` ranges.
/// - `reserved_regions`: Scratch slice that receives reserved ranges.
/// - `descriptors`: Descriptor buffer that receives the EFI-style memory map.
fn page_allocator_from_live_fdt<'a>(
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

/// Allocates a high-memory destination, copies the current firmware image, and
/// jumps into the relocated copy.
fn try_relocate_firmware(
    boot_hart: usize,
    device_tree: *const u8,
    entry_stack: usize,
) -> Option<()> {
    let mut regions = [MemoryRegion { base: 0, size: 0 }; 8];
    let mut reserved = [MemoryRegion { base: 0, size: 0 }; 16];
    let mut memory_map = [EMPTY_MEMORY_DESCRIPTOR; 32];
    let mut allocator = page_allocator_from_live_fdt(
        device_tree,
        &mut regions,
        &mut reserved,
        &mut memory_map,
    )?;

    let runtime_base = firmware_runtime_base();
    let runtime_size = firmware_runtime_size();
    let runtime_pages = runtime_size.div_ceil(memory::EFI_PAGE_SIZE as usize);
    let mut relocated_base = u64::MAX;
    allocator
        .AllocatePages(
            EFI_ALLOCATE_TYPE::AllocateMaxAddress,
            EFI_MEMORY_TYPE::EfiBootServicesData,
            runtime_pages,
            &mut relocated_base,
        )
        .ok()?;

    unsafe {
        ptr::copy_nonoverlapping(
            runtime_base as *const u8,
            relocated_base as *mut u8,
            runtime_size,
        );
    }

    crate::println!(
        "rustfimware: relocating image to {:#018x}",
        relocated_base as usize,
    );

    unsafe {
        enter_relocated_copy(
            relocated_base as usize,
            boot_hart,
            device_tree,
            entry_stack,
        )
    }
}

/// Transfers control into the relocated firmware image.
unsafe fn enter_relocated_copy(
    relocated_base: usize,
    boot_hart: usize,
    device_tree: *const u8,
    entry_stack: usize,
) -> ! {
    let relocated_entry_address = relocated_base
        .checked_add(relocated_entry_offset())
        .unwrap();

    unsafe {
        asm!(
            "fence.i",
            "jr {entry}",
            entry = in(reg) relocated_entry_address,
            in("a0") boot_hart,
            in("a1") device_tree as usize,
            in("a2") entry_stack,
            options(noreturn)
        );
    }
}

/// Returns the Linux root-device command line for one virtio block device and partition.
///
/// # Parameters
///
/// - `block_device_index`: Zero-based virtio block-device index.
/// - `partition_number`: One-based partition number selected for boot.
fn root_command_line<'a>(
    block_device_index: usize,
    partition_number: u32,
    buffer: &'a mut [u8; 24],
) -> Option<&'a str> {
    if block_device_index >= 26 {
        return None;
    }

    let prefix = b"root=/dev/vda";
    buffer[..prefix.len()].copy_from_slice(prefix);
    // Replace the trailing drive letter so vda/vdb/... tracks the VirtIO disk index.
    buffer[prefix.len() - 1] = b'a' + block_device_index as u8;

    let mut digits = [0u8; 10];
    let mut digit_count = 0usize;
    let mut value = partition_number;
    loop {
        digits[digit_count] = b'0' + (value % 10) as u8;
        digit_count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }

    let mut index = 0usize;
    while index < digit_count {
        buffer[prefix.len() + index] = digits[digit_count - 1 - index];
        index += 1;
    }

    Some(unsafe {
        str::from_utf8_unchecked(&buffer[..prefix.len() + digit_count])
    })
}

/// Empty descriptor value used for temporary EFI memory-map arrays.
const EMPTY_MEMORY_DESCRIPTOR: EFI_MEMORY_DESCRIPTOR = EFI_MEMORY_DESCRIPTOR {
    Type: 0,
    PhysicalStart: 0,
    VirtualStart: 0,
    NumberOfPages: 0,
    Attribute: 0,
};

/// Filesystem classification derived from probing one partition start sector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DetectedFilesystem {
    /// The partition mounted successfully as FAT.
    Fat,
    /// The partition mounted successfully as ext4.
    Ext4,
    /// The partition did not match the supported filesystem probes.
    Unknown,
}

/// Detects whether one partition contains a FAT filesystem, an ext4
/// filesystem, or neither.
///
/// # Parameters
///
/// - `device`: Block device that contains the partition.
/// - `partition_start_lba`: First logical block of the partition.
fn detect_partition_filesystem<D: BlockDevice>(
    device: &mut D,
    partition_start_lba: u64,
) -> DetectedFilesystem {
    if FatVolume::new(device, partition_start_lba).is_ok() {
        return DetectedFilesystem::Fat;
    }

    if Ext4Volume::new(device, partition_start_lba).is_ok() {
        return DetectedFilesystem::Ext4;
    }

    DetectedFilesystem::Unknown
}

/// Probes QEMU VirtIO block devices, prints GPT partition information, and
/// attempts Linux boot from boot-flagged partitions.
///
/// # Parameters
///
/// - `boot_hart`: Original hart identifier received in register `a0`.
/// - `device_tree_ptr`: Original device-tree pointer received in register `a1`.
fn probe_virtio(boot_hart: usize, device_tree_ptr: *const u8) {
    let mut found_any = false;
    let mut block_device_index = 0usize;

    for probe in qemu_virt_block_devices() {
        found_any = true;
        crate::println!(
            "virtio: block device at slot {} base {:#018x}",
            probe.slot,
            probe.device.base_address(),
        );

        let current_block_device_index = block_device_index;
        block_device_index += 1;

        let mut driver = match unsafe { VirtioBlockDriver::new(probe.device) } {
            Ok(driver) => driver,
            Err(_) => {
                crate::println!("gpt: virtio block init failed");
                continue;
            }
        };

        let mut partitions = match GptPartitionTable::new(&mut driver) {
            Some(partitions) => partitions,
            None => {
                crate::println!("gpt: no primary GPT header");
                continue;
            }
        };

        let partition_count = partitions.partition_count();
        let mut partition_index = 0;
        while partition_index < partition_count {
            let partition = match partitions.partition(partition_index) {
                Some(partition) => partition,
                None => break,
            };

            partition_index += 1;

            if !partition.is_present() {
                continue;
            }

            let mut label = [0u8; 72];
            let mut partition_type = [0u8; 36];
            let partition_start_lba = partition.first_lba();
            let bootable = partition.bootable();
            // Partition entries borrow the GPT table, so drop that view before
            // probing the same block device as FAT or ext4.
            drop(partitions);
            let filesystem = detect_partition_filesystem(
                &mut driver,
                partition_start_lba,
            );

            let filesystem_name = match filesystem {
                DetectedFilesystem::Fat => "fat",
                DetectedFilesystem::Ext4 => "ext4",
                DetectedFilesystem::Unknown => "unknown",
            };

            crate::println!(
                "partition {}: start={}, size={}, label='{}', type='{}', fs='{}', bootflag={}",
                partition_index,
                partition_start_lba,
                partition.sector_count(),
                partition.label(&mut label),
                partition.partition_type(&mut partition_type),
                filesystem_name,
                bootable,
            );

            if bootable {
                try_linux_boot_from_partition(
                    &mut driver,
                    partition,
                    filesystem,
                    current_block_device_index,
                    partition_index,
                    boot_hart,
                    device_tree_ptr,
                );
            }

            // Reopen the GPT view after direct filesystem probing/loading so
            // the next loop iteration can read partition metadata again.
            partitions = match GptPartitionTable::new(&mut driver) {
                Some(partitions) => partitions,
                None => {
                    crate::println!("gpt: failed to reopen partition table");
                    break;
                }
            };
        }
    }

    if !found_any {
        crate::println!("virtio: no block device found on qemu virt mmio");
    }
}

#[unsafe(no_mangle)]
/// Firmware entry point reached after early assembly stack setup.
///
/// # Parameters
///
/// - `boot_hart`: Original hart identifier received in register `a0`.
/// - `device_tree`: Original device-tree pointer received in register `a1`.
/// - `entry_stack`: Original stack pointer value observed before switching stacks.
extern "C" fn rust_entry(
    boot_hart: usize,
    device_tree: *const u8,
    entry_stack: usize,
) -> ! {
    run_firmware(boot_hart, device_tree, entry_stack)
}

#[unsafe(no_mangle)]
/// Firmware entry point reached after jumping into a relocated firmware copy.
///
/// # Parameters
///
/// - `boot_hart`: Original hart identifier received in register `a0`.
/// - `device_tree`: Original device-tree pointer received in register `a1`.
/// - `entry_stack`: Original stack pointer value observed before switching stacks.
extern "C" fn rust_relocated_entry(
    boot_hart: usize,
    device_tree: *const u8,
    entry_stack: usize,
) -> ! {
    run_firmware(boot_hart, device_tree, entry_stack)
}

/// Shared firmware main routine used by the primary and relocated entry paths.
///
/// # Parameters
///
/// - `boot_hart`: Original hart identifier received in register `a0`.
/// - `device_tree`: Original device-tree pointer received in register `a1`.
/// - `entry_stack`: Original stack pointer value observed before switching stacks.
fn run_firmware(
    boot_hart: usize,
    device_tree: *const u8,
    entry_stack: usize,
) -> ! {
    if boot_hart == 0 && firmware_runtime_base() == PRIMARY_FIRMWARE_LOAD_ADDRESS {
        if let Some(()) = try_relocate_firmware(boot_hart, device_tree, entry_stack) {
            unreachable!();
        }
        crate::println!(
            "rustfimware: relocation unavailable, continuing in-place"
        );
    }

    if firmware_runtime_base() != PRIMARY_FIRMWARE_LOAD_ADDRESS {
        diagnostics::print_rustfw_banner();
    }

    greet();
    print_diagnostics(boot_hart, device_tree, entry_stack);
    probe_virtio(boot_hart, device_tree);
    crate::println!("rustfimware: poweroff via sbi srst");
    poweroff()
}

#[panic_handler]
/// Handles panics by printing a message and powering off the machine.
///
/// # Parameters
///
/// - `_info`: Panic metadata supplied by the Rust core runtime.
fn panic(_info: &PanicInfo<'_>) -> ! {
    crate::println!("rustfimware: panic");
    poweroff_on_failure()
}