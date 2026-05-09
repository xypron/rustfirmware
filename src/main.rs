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
/// Generic partition-table interfaces for GPT and future MBR support.
pub mod partition;
/// Filesystem abstractions shared by FAT and future formats.
pub mod filesystem;
/// Linux boot-method request construction.
pub mod linux;
/// Read-only FAT filesystem support for loading files by path.
pub mod fat;
/// Boot-oriented device-tree object stub.
pub mod dtb;
/// Flattened device tree parsing helpers used for diagnostics and future edits.
pub mod devicetree;
/// Firmware diagnostics output and reporting.
pub mod diagnostics;
/// EFI-style page allocator and memory map support.
pub mod memory;

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
use core::ptr;
use core::str;
use devicetree::{Fdt, MemoryRegion};
use diagnostics::print_diagnostics;
use dtb::Dtb;
use fat::FatVolume;
use filesystem::{FileHandle, FileInfo, FileInfoView, FileSystem, LoadedFile};
use gpt::GptPartitionTable;
use linux::{boot as linux_boot, check_kernel_header};
use memory::{EFI_MEMORY_DESCRIPTOR, PageAllocator};
use partition::{PartitionEntry, PartitionTable};
use virtio::{qemu_virt_block_devices, BlockDevice};
use virtio::VirtioBlockDriver;

unsafe extern "C" {
    /// Linker-defined top of the firmware-owned runtime stack.
    static __stack_top: u8;
}

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
/// Boot hart identifier captured from register `a0` at firmware entry.
static mut BOOT_HART_ID: usize = 0;

#[unsafe(no_mangle)]
/// Flattened device-tree pointer captured from register `a1` at firmware entry.
static mut DEVICE_TREE_PTR: usize = 0;

#[unsafe(no_mangle)]
/// Stack pointer observed on entry before switching to the firmware stack.
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
    core::ptr::addr_of!(__stack_top) as usize
}

/// Performs one SBI environment call with the provided register arguments.
///
/// # Parameters
///
/// - `arg0`: Value loaded into register `a0` before the call.
/// - `arg1`: Value loaded into register `a1` before the call.
/// - `arg2`: Value loaded into register `a2` before the call.
/// - `arg3`: Value loaded into register `a3` before the call.
/// - `arg4`: Value loaded into register `a4` before the call.
/// - `arg5`: Value loaded into register `a5` before the call.
/// - `fid`: SBI function identifier loaded into register `a6`.
/// - `eid`: SBI extension identifier loaded into register `a7`.
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

/// Writes one string slice through the SBI debug console extension.
///
/// # Parameters
///
/// - `message`: Text buffer passed to the SBI DBCN console write call.
pub(crate) fn puts(message: &str) -> SbiRet {
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

/// Stops forward progress by repeatedly waiting for interrupts.
///
/// # Parameters
///
/// This function does not accept parameters.
fn halt() -> ! {
    loop {
        unsafe {
            asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Requests an SBI system reset and halts if the call returns.
///
/// # Parameters
///
/// - `reset_type`: SBI reset type passed to the SRST extension.
/// - `reset_reason`: SBI reset reason passed to the SRST extension.
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

/// Powers off the machine through the SBI system reset extension.
///
/// # Parameters
///
/// This function does not accept parameters.
fn poweroff() -> ! {
    system_reset(SBI_SRST_RESET_TYPE_SHUTDOWN, SBI_SRST_RESET_REASON_NONE)
}

/// Prints the firmware name, version, and build profile.
///
/// # Parameters
///
/// This function does not accept parameters.
fn greet() -> SbiRet {
    let _ = puts(env!("CARGO_PKG_NAME"));
    let _ = puts(" ");
    let _ = puts(env!("CARGO_PKG_VERSION"));
    let _ = puts(" (");
    let _ = puts(BUILD_PROFILE);
    puts(")\n")
}

/// Prints one `usize` value as a fixed-width hexadecimal number.
///
/// # Parameters
///
/// - `value`: Machine-sized integer to format and emit.
pub(crate) fn put_hex_usize(value: usize) {
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

/// Prints one decimal digit without additional formatting.
///
/// # Parameters
///
/// - `value`: Single digit value to emit.
pub(crate) fn put_small_decimal(value: usize) {
    let digit = [b'0' + value as u8];
    let text = unsafe { str::from_utf8_unchecked(&digit) };
    let _ = puts(text);
}

/// Prints one `u64` value in decimal form.
///
/// # Parameters
///
/// - `value`: Unsigned integer value to format and emit.
pub(crate) fn put_decimal_u64(mut value: u64) {
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

/// Prints one boolean value as `true` or `false`.
///
/// # Parameters
///
/// - `value`: Boolean value to emit.
fn put_bool(value: bool) {
    if value {
        let _ = puts("true");
    } else {
        let _ = puts("false");
    }
}

/// Mounts one FAT ESP and prints every file path plus size.
///
/// # Parameters
///
/// - `device`: Block device that contains the ESP.
/// - `partition_start_lba`: First logical block of the ESP.
fn list_esp_files<D: BlockDevice>(
    device: &mut D,
    partition_start_lba: u64,
    block_device_index: usize,
) {
    let mut volume = match FatVolume::new(device, partition_start_lba) {
        Ok(volume) => volume,
        Err(_) => {
            let _ = puts("fat: failed to mount esp\n");
            return;
        }
    };

    let result = volume.walk_files(|path, size| {
        let _ = puts("fat: file '");
        let _ = puts(path);
        let _ = puts("', size=");
        put_decimal_u64(size as u64);
        let _ = puts("\n");
    });

    if result.is_err() {
        let _ = puts("fat: failed to walk esp files\n");
    }

    try_linux_boot_from_esp(&mut volume, block_device_index);
}

/// Tries the Linux boot method when `/vmlinux` and `/initrd.img` exist.
///
/// # Parameters
///
/// - `volume`: Mounted ESP filesystem.
/// - `block_device_index`: Zero-based virtio block-device index.
fn try_linux_boot_from_esp<D: BlockDevice>(
    volume: &mut FatVolume<'_, D>,
    block_device_index: usize,
) {
    let Some((kernel_path, kernel)) = describe_first_file(
        volume,
        &["/vmlinux", "/vmlinuz"],
    ) else {
        return;
    };
    let Some(initrd) = describe_file(volume, "/initrd.img") else {
        return;
    };
    let Some(command_line) = root_command_line(block_device_index) else {
        let _ = puts("linux: unsupported root device index\n");
        return;
    };

    let device_tree = Dtb::new();
    match linux_boot(&kernel, Some(&initrd), &device_tree, command_line) {
        Ok(request) => {
            let mut regions = [MemoryRegion { base: 0, size: 0 }; 8];
            let mut reserved = [MemoryRegion { base: 0, size: 0 }; 16];
            let mut memory_map = [EMPTY_MEMORY_DESCRIPTOR; 32];
            let mut allocator = match page_allocator_from_live_fdt(
                &mut regions,
                &mut reserved,
                &mut memory_map,
            ) {
                Some(allocator) => allocator,
                None => {
                    let _ = puts("linux: page allocator unavailable\n");
                    return;
                }
            };
            let kernel_loaded = match load_file(volume, kernel_path, &mut allocator) {
                Some(file) => file,
                None => {
                    let _ = puts("linux: failed to load ");
                    let _ = puts(kernel_path);
                    let _ = puts("\n");
                    return;
                }
            };
            match check_kernel_header(&kernel_loaded) {
                Ok(()) => {
                    let _ = puts("linux: kernel object ");
                    let _ = puts(kernel_path);
                    let _ = puts(" matches RISC-V boot image header\n");
                }
                Err(_) => {
                    let _ = puts("linux: kernel object ");
                    let _ = puts(kernel_path);
                    let _ = puts(" does not match RISC-V boot image header\n");
                    return;
                }
            }
            let initrd_loaded = match load_file(volume, "/initrd.img", &mut allocator) {
                Some(file) => file,
                None => {
                    let _ = puts("linux: failed to load /initrd.img\n");
                    return;
                }
            };
            let _ = puts("linux: boot request invoked ");
            let _ = puts(kernel_path);
            let _ = puts("=");
            put_decimal_u64(request.kernel_size_bytes() as u64);
            let _ = puts(" @ ");
            put_hex_usize(kernel_loaded.physical_start() as usize);
            let _ = puts(", /initrd.img=");
            put_decimal_u64(request.initrd_size_bytes().unwrap_or(0) as u64);
            let _ = puts(" @ ");
            put_hex_usize(initrd_loaded.physical_start() as usize);
            let _ = puts(", cmdline='");
            let _ = puts(request.command_line());
            let _ = puts("'\n");
        }
        Err(_) => {
            let _ = puts("linux: boot request rejected\n");
        }
    }
}

/// Returns detached metadata for one path inside `volume`.
///
/// # Parameters
///
/// - `volume`: Mounted filesystem containing the path.
/// - `path`: Absolute path to inspect.
fn describe_file<D: BlockDevice>(
    volume: &mut FatVolume<'_, D>,
    path: &str,
) -> Option<FileInfo> {
    let file = volume.open(path).ok()?;
    Some(file.info())
}

/// Returns detached metadata for the first existing path in `paths`.
///
/// # Parameters
///
/// - `volume`: Mounted filesystem containing the candidate paths.
/// - `paths`: Ordered candidate paths to inspect.
fn describe_first_file<'a, D: BlockDevice>(
    volume: &mut FatVolume<'_, D>,
    paths: &'a [&'a str],
) -> Option<(&'a str, FileInfo)> {
    let mut index = 0usize;
    while index < paths.len() {
        let path = paths[index];
        if let Some(info) = describe_file(volume, path) {
            return Some((path, info));
        }

        index += 1;
    }

    None
}

/// Loads one file from `volume` into EFI-style pages.
///
/// # Parameters
///
/// - `volume`: Mounted filesystem containing the path.
/// - `path`: Absolute path to load.
/// - `allocator`: Page allocator used to reserve the destination pages.
fn load_file<D: BlockDevice>(
    volume: &mut FatVolume<'_, D>,
    path: &str,
    allocator: &mut PageAllocator<'_>,
) -> Option<LoadedFile> {
    let mut file = volume.open(path).ok()?;
    file.load(allocator).ok()
}

/// Builds a page allocator from the live boot-time device tree.
///
/// # Parameters
///
/// - `memory_regions`: Scratch slice that receives `/memory` ranges.
/// - `reserved_regions`: Scratch slice that receives reserved ranges.
/// - `descriptors`: Descriptor buffer that receives the EFI-style memory map.
fn page_allocator_from_live_fdt<'a>(
    memory_regions: &mut [MemoryRegion],
    reserved_regions: &mut [MemoryRegion],
    descriptors: &'a mut [EFI_MEMORY_DESCRIPTOR],
) -> Option<PageAllocator<'a>> {
    let fdt = unsafe { Fdt::from_ptr(device_tree_ptr()).ok()? };
    PageAllocator::from_fdt(
        &fdt,
        memory_regions,
        reserved_regions,
        descriptors,
    )
    .ok()
}

/// Returns the Linux root-device command line for one virtio block device.
///
/// # Parameters
///
/// - `block_device_index`: Zero-based virtio block-device index.
fn root_command_line(block_device_index: usize) -> Option<&'static str> {
    const ROOT_COMMAND_LINES: [&str; 26] = [
        "root=/dev/vda", "root=/dev/vdb", "root=/dev/vdc",
        "root=/dev/vdd", "root=/dev/vde", "root=/dev/vdf",
        "root=/dev/vdg", "root=/dev/vdh", "root=/dev/vdi",
        "root=/dev/vdj", "root=/dev/vdk", "root=/dev/vdl",
        "root=/dev/vdm", "root=/dev/vdn", "root=/dev/vdo",
        "root=/dev/vdp", "root=/dev/vdq", "root=/dev/vdr",
        "root=/dev/vds", "root=/dev/vdt", "root=/dev/vdu",
        "root=/dev/vdv", "root=/dev/vdw", "root=/dev/vdx",
        "root=/dev/vdy", "root=/dev/vdz",
    ];

    ROOT_COMMAND_LINES.get(block_device_index).copied()
}

/// Empty descriptor value used for temporary EFI memory-map arrays.
const EMPTY_MEMORY_DESCRIPTOR: EFI_MEMORY_DESCRIPTOR = EFI_MEMORY_DESCRIPTOR {
    Type: 0,
    PhysicalStart: 0,
    VirtualStart: 0,
    NumberOfPages: 0,
    Attribute: 0,
};

/// Probes QEMU VirtIO block devices and prints GPT partition information.
///
/// # Parameters
///
/// This function does not accept parameters.
fn probe_virtio() {
    let mut found_any = false;
    let mut block_device_index = 0usize;

    for probe in qemu_virt_block_devices() {
        found_any = true;
        let _ = puts("virtio: block device at slot ");
        put_small_decimal(probe.slot);
        let _ = puts(" base ");
        put_hex_usize(probe.device.base_address());
        let _ = puts("\n");

        let current_block_device_index = block_device_index;
        block_device_index += 1;

        let mut driver = match unsafe { VirtioBlockDriver::new(probe.device) } {
            Ok(driver) => driver,
            Err(_) => {
                let _ = puts("gpt: virtio block init failed\n");
                continue;
            }
        };

        let mut partitions = match GptPartitionTable::new(&mut driver) {
            Some(partitions) => partitions,
            None => {
                let _ = puts("gpt: no primary GPT header\n");
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

            let _ = puts("partition ");
            put_decimal_u64(partition_index as u64);
            let _ = puts(": start=");
            put_decimal_u64(partition.first_lba());
            let _ = puts(", size=");
            put_decimal_u64(partition.sector_count());
            let _ = puts(", label='");
            let _ = puts(partition.label(&mut label));
            let _ = puts("', type='");
            let _ = puts(partition.partition_type(&mut partition_type));
            let _ = puts("', bootflag=");
            put_bool(partition.bootable());
            let _ = puts("\n");

            if partition.is_efi_system_partition() {
                let partition_start_lba = partition.first_lba();
                let _ = puts("fat: walking esp files\n");
                drop(partitions);
                list_esp_files(
                    &mut driver,
                    partition_start_lba,
                    current_block_device_index,
                );

                partitions = match GptPartitionTable::new(&mut driver) {
                    Some(partitions) => partitions,
                    None => {
                        let _ = puts("gpt: failed to reopen partition table\n");
                        break;
                    }
                };
            }
        }
    }

    if !found_any {
        let _ = puts("virtio: no block device found on qemu virt mmio\n");
    }
}

#[unsafe(no_mangle)]
/// Firmware entry point reached after early assembly stack setup.
///
/// # Parameters
///
/// This function does not accept parameters.
extern "C" fn rust_entry() -> ! {
    let _boot_hart = boot_hart_id();
    let _device_tree = device_tree_ptr();

    let _ = greet();
    print_diagnostics();
    probe_virtio();
    let _ = puts("rustfimware: poweroff via sbi srst\n");
    poweroff()
}

#[panic_handler]
/// Handles panics by printing a message and powering off the machine.
///
/// # Parameters
///
/// - `_info`: Panic metadata supplied by the Rust core runtime.
fn panic(_info: &PanicInfo<'_>) -> ! {
    let _ = puts("rustfimware: panic\n");
    system_reset(SBI_SRST_RESET_TYPE_SHUTDOWN, SBI_SRST_RESET_REASON_SYSTEM_FAILURE)
}