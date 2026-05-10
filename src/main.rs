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
/// S-mode trap vector setup and trap handling.
pub mod interrupts;
/// OpenSBI constants and environment-call wrappers.
pub mod sbi;
/// EFI-style page allocator and memory map support.
pub mod memory;

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
use core::ptr;
use devicetree::MemoryRegion;
use diagnostics::print_diagnostics;
use filesystem::{detect_partition_filesystem, DetectedFilesystem};
use gpt::GptPartitionTable;
use interrupts::{install_smode_trap_vector, smode_trap_vector_offset};
use linux::{try_boot_from_partition as linux_try_boot_from_partition, LinuxBootFilesystem};
use memory::{
    page_allocator_from_live_fdt, EFI_ALLOCATE_TYPE, EFI_MEMORY_TYPE,
    EMPTY_MEMORY_DESCRIPTOR,
};
use partition::{PartitionEntry, PartitionTable};
use sbi::{poweroff, poweroff_on_failure};
use virtio::qemu_virt_block_devices;
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

/// Returns the runtime address of the active image trap vector.
///
/// # Parameters
///
/// This function does not accept parameters.
fn trap_vector_address() -> usize {
    firmware_runtime_base()
        .checked_add(smode_trap_vector_offset())
        .unwrap()
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

            let linux_filesystem = match filesystem {
                DetectedFilesystem::Fat => LinuxBootFilesystem::Fat,
                DetectedFilesystem::Ext4 => LinuxBootFilesystem::Ext4,
                DetectedFilesystem::Unknown => LinuxBootFilesystem::Unknown,
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
                linux_try_boot_from_partition(
                    &mut driver,
                    partition,
                    linux_filesystem,
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
    install_smode_trap_vector(trap_vector_address());

    if boot_hart == 0 && firmware_runtime_base() == PRIMARY_FIRMWARE_LOAD_ADDRESS {
        if let Some(()) = try_relocate_firmware(boot_hart, device_tree, entry_stack) {
            unreachable!();
        }
        crate::println!("rustfimware: relocation unavailable, powering off");
        poweroff();
    }

    if firmware_runtime_base() != PRIMARY_FIRMWARE_LOAD_ADDRESS {
        install_smode_trap_vector(trap_vector_address());
        diagnostics::print_rustfw_banner();
    }

    greet();
    print_diagnostics(boot_hart, device_tree);
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