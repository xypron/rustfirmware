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
pub mod dtb_write;
/// Flattened device tree parsing helpers used for diagnostics and future edits.
pub mod dtb_read;
/// Device-tree memory-region interpretation helpers.
pub mod dtb_memory;
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
use dtb_memory::MemoryRegion;
use diagnostics::{greet, print_diagnostics};
use diagnostics::print_memory_layout;
use interrupts::{install_smode_trap_vector, trap_vector_address};
use memory::{
    page_allocator_from_live_fdt, EFI_ALLOCATE_TYPE, EFI_MEMORY_TYPE,
    EMPTY_MEMORY_DESCRIPTOR,
};
use sbi::{poweroff, poweroff_on_failure};

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
    let mut allocator = unsafe {
        page_allocator_from_live_fdt(
            device_tree,
            &mut regions,
            &mut reserved,
            &mut memory_map,
        )
    }?;

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
    install_smode_trap_vector(trap_vector_address(firmware_runtime_base()));

    if firmware_runtime_base() == PRIMARY_FIRMWARE_LOAD_ADDRESS {
        if let Some(()) = try_relocate_firmware(boot_hart, device_tree, entry_stack) {
            unreachable!();
        }
        crate::println!("rustfimware: relocation unavailable, powering off");
        poweroff();
    }

    if firmware_runtime_base() != PRIMARY_FIRMWARE_LOAD_ADDRESS {
        diagnostics::print_rustfw_banner();
    }

    greet();
    print_diagnostics(boot_hart, device_tree);
    if matches!(option_env!("RUSTFW_PRINT_MEMORY_LAYOUT"), Some("1")) {
        unsafe {
            print_memory_layout(device_tree);
        }
    }
    unsafe {
        virtio::probe_virtio(boot_hart, device_tree);
    }
    crate::println!("rustfimware: poweroff via sbi srst");
    poweroff()
}

#[panic_handler]
/// Handles panics by printing a message and powering off the machine.
///
/// # Parameters
///
/// - `info`: Panic metadata supplied by the Rust core runtime.
fn panic(info: &PanicInfo<'_>) -> ! {
    crate::println!("rustfimware: panic");
    if let Some(location) = info.location() {
        crate::println!(
            "rustfimware: panic location {}:{}",
            location.file(),
            location.line(),
        );
    }
    poweroff_on_failure()
}