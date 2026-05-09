#![no_std]
#![no_main]

//! Freestanding RISC-V firmware entry point.
//!
//! The binary boots under OpenSBI, captures the boot hart and incoming device
//! tree pointer, initializes a fixed stack at the firmware load address, and
//! then prints diagnostics plus simple storage information through the SBI DBCN
//! console extension.

/// VirtIO MMIO transport, queue, and block-device support.
pub mod virtio;
/// GUID partition table parsing for block devices.
pub mod gpt;
/// Flattened device tree parsing helpers used for diagnostics and future edits.
pub mod devicetree;

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
use core::ptr;
use core::str;
use devicetree::{Fdt, MemoryRegion};
use gpt::{read_partition_entry, read_primary_header};
use virtio::qemu_virt_block_devices;
use virtio::VirtioBlockDriver;

/// SBI extension ID for the debug console extension.
const SBI_EXT_DBCN: usize = 0x4442_434e;
/// SBI function ID for buffered debug console writes.
const SBI_DBCN_CONSOLE_WRITE: usize = 0;
/// SBI extension ID for the system reset extension.
const SBI_EXT_SRST: usize = 0x5352_5354;
/// SBI function ID for system reset requests.
const SBI_SRST_SYSTEM_RESET: usize = 0;
/// SRST reset type used to power off the machine.
const SBI_SRST_RESET_TYPE_SHUTDOWN: usize = 0;
/// SRST reset reason for a normal shutdown.
const SBI_SRST_RESET_REASON_NONE: usize = 0;
/// SRST reset reason for a firmware-detected failure.
const SBI_SRST_RESET_REASON_SYSTEM_FAILURE: usize = 1;
/// Top of the fixed firmware-owned stack used after early entry.
const STACK_TOP: usize = 0x8020_0000;
/// Build profile name injected by the Makefile for runtime diagnostics.
const BUILD_PROFILE: &str = match option_env!("PROFILE_NAME") {
    Some(profile) => profile,
    None => "unknown",
};

global_asm!(
    r#"
    .section .text.entry
    .globl _start
_start:
    la t0, BOOT_HART_ID
    sd a0, 0(t0)
    la t0, DEVICE_TREE_PTR
    sd a1, 0(t0)
    la t0, ENTRY_STACK_PTR
    sd sp, 0(t0)
    li sp, 0x80200000
    tail rust_entry
"#
);

#[unsafe(no_mangle)]
static mut BOOT_HART_ID: usize = 0;

#[unsafe(no_mangle)]
static mut DEVICE_TREE_PTR: usize = 0;

#[unsafe(no_mangle)]
static mut ENTRY_STACK_PTR: usize = 0;

/// SBI return pair carrying an error code and one return value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
struct SbiRet {
    /// SBI error code returned in register `a0`.
    error: usize,
    /// SBI result value returned in register `a1`.
    value: usize,
}

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

fn ecall(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
    fid: usize,
    eid: usize,
) -> SbiRet {
    let error;
    let value;

    unsafe {
        asm!(
            "ecall",
            inlateout("a0") arg0 => error,
            inlateout("a1") arg1 => value,
            in("a2") arg2,
            in("a3") arg3,
            in("a4") arg4,
            in("a5") arg5,
            in("a6") fid,
            in("a7") eid,
            options(nostack)
        );
    }

    SbiRet { error, value }
}

fn puts(message: &str) -> SbiRet {
    ecall(
        message.len(),
        message.as_ptr() as usize,
        0,
        0,
        0,
        0,
        SBI_DBCN_CONSOLE_WRITE,
        SBI_EXT_DBCN,
    )
}

fn halt() -> ! {
    loop {
        unsafe {
            asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

fn system_reset(reset_type: usize, reset_reason: usize) -> ! {
    let _ = ecall(
        reset_type,
        reset_reason,
        0,
        0,
        0,
        0,
        SBI_SRST_SYSTEM_RESET,
        SBI_EXT_SRST,
    );

    halt()
}

fn poweroff() -> ! {
    system_reset(SBI_SRST_RESET_TYPE_SHUTDOWN, SBI_SRST_RESET_REASON_NONE)
}

fn greet() -> SbiRet {
    let _ = puts(env!("CARGO_PKG_NAME"));
    let _ = puts(" ");
    let _ = puts(env!("CARGO_PKG_VERSION"));
    let _ = puts(" (");
    let _ = puts(BUILD_PROFILE);
    puts(")\n")
}

fn diagnostics() {
    let _ = puts("diagnostics: boot_hart=");
    put_decimal_u64(boot_hart_id() as u64);
    let _ = puts(", entry_sp=");
    put_hex_usize(entry_stack_ptr());
    let _ = puts(", stack_top=");
    put_hex_usize(STACK_TOP);
    let _ = puts("\n");

    let mut regions = [MemoryRegion { base: 0, size: 0 }; 8];
    let mut reserved = [MemoryRegion { base: 0, size: 0 }; 16];
    let (region_count, reserved_count) = match unsafe { Fdt::from_ptr(device_tree_ptr()) } {
        Ok(fdt) => (fdt.memory_regions(&mut regions), fdt.reserved_regions(&mut reserved)),
        Err(_) => {
            let _ = puts("diagnostics: memory-map unavailable\n");
            return;
        }
    };

    let mut index = 0usize;
    while index < region_count {
        let _ = puts("memory ");
        put_decimal_u64((index + 1) as u64);
        let _ = puts(": base=");
        put_hex_usize(regions[index].base as usize);
        let _ = puts(", size=");
        put_hex_usize(regions[index].size as usize);
        let _ = puts("\n");
        index += 1;
    }

    index = 0;
    while index < reserved_count {
        let _ = puts("reserved ");
        put_decimal_u64((index + 1) as u64);
        let _ = puts(": base=");
        put_hex_usize(reserved[index].base as usize);
        let _ = puts(", size=");
        put_hex_usize(reserved[index].size as usize);
        let _ = puts("\n");
        index += 1;
    }
}

fn put_hex_usize(value: usize) {
    /// Hex digit lookup table used for manual number formatting.
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

    let mut buffer = [0u8; 2 + (core::mem::size_of::<usize>() * 2)];
    let mut shift = (core::mem::size_of::<usize>() * 8) as isize - 4;

    buffer[0] = b'0';
    buffer[1] = b'x';

    let mut index = 2;
    while shift >= 0 {
        let nibble = ((value >> shift as usize) & 0xf) as usize;
        buffer[index] = HEX_DIGITS[nibble];
        index += 1;
        shift -= 4;
    }

    let text = unsafe { str::from_utf8_unchecked(&buffer) };
    let _ = puts(text);
}

fn put_small_decimal(value: usize) {
    let digit = [b'0' + value as u8];
    let text = unsafe { str::from_utf8_unchecked(&digit) };
    let _ = puts(text);
}

fn put_decimal_u64(mut value: u64) {
    let mut buffer = [0u8; 20];
    let mut index = buffer.len();

    if value == 0 {
        let _ = puts("0");
        return;
    }

    while value != 0 {
        index -= 1;
        buffer[index] = b'0' + (value % 10) as u8;
        value /= 10;
    }

    let text = unsafe { str::from_utf8_unchecked(&buffer[index..]) };
    let _ = puts(text);
}

fn put_bool(value: bool) {
    if value {
        let _ = puts("true");
    } else {
        let _ = puts("false");
    }
}

fn probe_virtio() {
    let mut found_any = false;

    for probe in qemu_virt_block_devices() {
        found_any = true;
        let _ = puts("virtio: block device at slot ");
        put_small_decimal(probe.slot);
        let _ = puts(" base ");
        put_hex_usize(probe.device.base_address());
        let _ = puts("\n");

        let mut driver = match unsafe { VirtioBlockDriver::new(probe.device) } {
            Ok(driver) => driver,
            Err(_) => {
                let _ = puts("gpt: virtio block init failed\n");
                continue;
            }
        };

        let header = match read_primary_header(&mut driver) {
            Some(header) => header,
            None => {
                let _ = puts("gpt: no primary GPT header\n");
                continue;
            }
        };

        let mut partition_index = 0;
        while partition_index < header.partition_entry_count {
            let partition = match read_partition_entry(&mut driver, &header, partition_index) {
                Some(partition) => partition,
                None => break,
            };

            partition_index += 1;

            if partition.is_unused() {
                continue;
            }

            let mut label = [0u8; 72];
            let mut partition_type = [0u8; 36];

            let _ = puts("partition ");
            put_decimal_u64(partition_index as u64);
            let _ = puts(": start=");
            put_decimal_u64(partition.first_lba);
            let _ = puts(", size=");
            put_decimal_u64(partition.sector_count());
            let _ = puts(", label='");
            let _ = puts(partition.label(&mut label));
            let _ = puts("', type='");
            let _ = puts(partition.partition_type(&mut partition_type));
            let _ = puts("', bootflag=");
            put_bool(partition.bootable());
            let _ = puts("\n");
        }
    }

    if !found_any {
        let _ = puts("virtio: no block device found on qemu virt mmio\n");
    }
}

#[unsafe(no_mangle)]
extern "C" fn rust_entry() -> ! {
    let _boot_hart = boot_hart_id();
    let _device_tree = device_tree_ptr();

    let _ = greet();
    diagnostics();
    probe_virtio();
    let _ = puts("rustfimware: poweroff via sbi srst\n");
    poweroff()
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    let _ = puts("rustfimware: panic\n");
    system_reset(SBI_SRST_RESET_TYPE_SHUTDOWN, SBI_SRST_RESET_REASON_SYSTEM_FAILURE)
}