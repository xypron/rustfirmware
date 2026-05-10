//! Linux boot-method request construction.
//!
//! This module validates the boot artifacts required for one Linux boot and
//! packages them into a Linux-specific request. The firmware loads the kernel
//! and optional initrd, clones and updates the device tree, validates the
//! RISC-V Linux boot header, and can then transfer control into the kernel.

use crate::dtb::{Dtb, DtbError};
use crate::filesystem::{FileInfoView, FileType, LoadedFile};
use crate::memory::PageAllocator;
use core::arch::asm;
use core::mem::offset_of;

/// RISC-V Linux boot image header size in bytes.
const RISCV_LINUX_HEADER_SIZE: usize = 64;
/// Extra space reserved in the cloned Linux device tree.
const LINUX_DTB_EXTRA_SIZE: usize = 8 * 1024;
/// Deprecated RISC-V Linux boot header magic value, little-endian.
const RISCV_LINUX_MAGIC: u64 = 0x5643_5349_52;
/// Required RISC-V Linux boot header second magic value, little-endian.
const RISCV_LINUX_MAGIC2: u32 = 0x0543_5352;

/// Parsed RISC-V Linux boot image header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
struct LinuxBootImageHeader {
    /// Executable code word 0.
    code0: u32,
    /// Executable code word 1.
    code1: u32,
    /// Image load offset in little-endian form.
    text_offset: u64,
    /// Effective image size in little-endian form.
    image_size: u64,
    /// Kernel flags in little-endian form.
    flags: u64,
    /// Boot header version.
    version: u32,
    /// Reserved field that must be zero in the documented layout.
    res1: u32,
    /// Reserved field that must be zero in the documented layout.
    res2: u64,
    /// Deprecated magic number, little-endian, "RISCV".
    magic: u64,
    /// Replacement magic number, little-endian, "RSC\x05".
    magic2: u32,
    /// Reserved field for the PE/COFF header offset.
    res3: u32,
}

/// Compile-time check that the boot header layout matches the spec.
const _: [(); 64] = [(); core::mem::size_of::<LinuxBootImageHeader>()];
/// Compile-time check that `magic` sits at offset `0x30`.
const _: [(); 0x30] = [(); offset_of!(LinuxBootImageHeader, magic)];
/// Compile-time check that `magic2` sits at offset `0x38`.
const _: [(); 0x38] = [(); offset_of!(LinuxBootImageHeader, magic2)];
/// Compile-time check that `res3` sits at offset `0x3c`.
const _: [(); 0x3c] = [(); offset_of!(LinuxBootImageHeader, res3)];

/// Linux boot request built from already-selected boot artifacts.
pub struct LinuxBootRequest<'a> {
    /// Device tree passed to the Linux boot flow.
    device_tree: Dtb,
    /// Kernel command line passed to Linux.
    command_line: &'a str,
    /// Size in bytes of the selected kernel image.
    kernel_size_bytes: usize,
    /// Size in bytes of the selected initrd image, if one is present.
    initrd_size_bytes: Option<usize>,
}

impl<'a> LinuxBootRequest<'a> {
    /// Returns the device tree selected for this Linux boot.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn device_tree(&self) -> &Dtb {
        &self.device_tree
    }

    /// Returns the Linux command line.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn command_line(&self) -> &'a str {
        self.command_line
    }

    /// Returns the size in bytes of the selected kernel image.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn kernel_size_bytes(&self) -> usize {
        self.kernel_size_bytes
    }

    /// Returns the size in bytes of the selected initrd image, if any.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub fn initrd_size_bytes(&self) -> Option<usize> {
        self.initrd_size_bytes
    }

    /// Fills Linux-specific `/chosen` properties in the cloned device tree.
    ///
    /// # Parameters
    ///
    /// - `initrd`: Optional loaded initrd image whose physical range should be exposed.
    /// - `command_line`: Linux kernel command line written into `/chosen/bootargs`.
    pub fn update_device_tree(
        &mut self,
        initrd: Option<&LoadedFile>,
        command_line: &str,
    ) -> Result<(), LinuxBootError> {
        self.device_tree
            .create_node("/chosen")
            .map_err(LinuxBootError::DeviceTreeUpdate)?;
        if let Some(initrd) = initrd {
            let initrd_start = initrd.physical_start();
            let initrd_size = u64::try_from(initrd.size_bytes())
                .map_err(|_| LinuxBootError::InitrdRangeOverflow)?;
            let initrd_end = initrd_start
                .checked_add(initrd_size)
                .ok_or(LinuxBootError::InitrdRangeOverflow)?;

            self.device_tree
                .set_property_u64("/chosen", "linux,initrd-start", initrd_start)
                .map_err(LinuxBootError::DeviceTreeUpdate)?;
            self.device_tree
                .set_property_u64("/chosen", "linux,initrd-end", initrd_end)
                .map_err(LinuxBootError::DeviceTreeUpdate)?;
        }
        self.device_tree
            .set_property_string("/chosen", "bootargs", command_line)
            .map_err(LinuxBootError::DeviceTreeUpdate)?;

        Ok(())
    }
}

/// Errors returned while constructing one Linux boot request.
pub enum LinuxBootError {
    /// The selected kernel artifact was not a regular file.
    KernelIsDirectory,
    /// The selected initrd artifact was not a regular file.
    InitrdIsDirectory,
    /// The loaded kernel image is smaller than the fixed boot header.
    KernelImageTooSmall,
    /// The loaded kernel image header had an unexpected `magic` value.
    InvalidKernelMagic,
    /// The loaded kernel image header had an unexpected `magic2` value.
    InvalidKernelMagic2,
    /// The loaded initrd range overflowed the supported 64-bit address space.
    InitrdRangeOverflow,
    /// Cloning the device tree for Linux boot failed.
    DeviceTreeClone(DtbError),
    /// Updating the cloned device tree for Linux boot failed.
    DeviceTreeUpdate(DtbError),
}

/// Builds one Linux boot request from already-selected boot artifacts.
///
/// Use `None` for `initrd` when the boot flow does not include an initrd.
/// The function intentionally accepts generic file handles rather than one
/// specific filesystem implementation.
///
/// # Parameters
///
/// - `kernel`: Opened file handle for the Linux kernel image.
/// - `initrd`: Optional opened file handle for the initrd image.
/// - `device_tree`: Device tree object passed to the Linux boot flow.
/// - `allocator`: Page allocator used to clone the device tree for Linux.
/// - `command_line`: Linux kernel command line.
pub fn boot<'a, Kernel, Initrd>(
    kernel: &Kernel,
    initrd: Option<&Initrd>,
    device_tree: &Dtb,
    allocator: &mut PageAllocator<'_>,
    command_line: &'a str,
) -> Result<LinuxBootRequest<'a>, LinuxBootError>
where
    Kernel: FileInfoView,
    Initrd: FileInfoView,
{
    if kernel.file_type() != FileType::File {
        return Err(LinuxBootError::KernelIsDirectory);
    }

    let initrd_size_bytes = match initrd {
        Some(initrd_file) => {
            if initrd_file.file_type() != FileType::File {
                return Err(LinuxBootError::InitrdIsDirectory);
            }

            Some(initrd_file.size_bytes())
        }
        None => None,
    };

    let cloned_device_tree = device_tree
        .clone(
            device_tree
                .size()
                .checked_add(LINUX_DTB_EXTRA_SIZE)
                .ok_or(LinuxBootError::DeviceTreeClone(DtbError::SizeOverflow))?,
            allocator,
        )
        .map_err(LinuxBootError::DeviceTreeClone)?;

    Ok(LinuxBootRequest {
        device_tree: cloned_device_tree,
        command_line,
        kernel_size_bytes: kernel.size_bytes(),
        initrd_size_bytes,
    })
}

/// Validates the RISC-V Linux boot-image header magic fields.
///
/// The current firmware checks both the deprecated `magic` field and the
/// replacement `magic2` field to ensure the loaded image looks like a Linux
/// RISC-V kernel image before later boot steps are attempted.
///
/// # Parameters
///
/// - `kernel_image`: Loaded kernel image whose header should be validated.
pub fn check_kernel_header(
    kernel_image: &LoadedFile,
) -> Result<(), LinuxBootError> {
    let bytes = kernel_image.bytes();
    dump_kernel_header(bytes);

    let header = parse_linux_boot_image_header(bytes)?;

    if header.magic != RISCV_LINUX_MAGIC {
        return Err(LinuxBootError::InvalidKernelMagic);
    }

    if header.magic2 != RISCV_LINUX_MAGIC2 {
        return Err(LinuxBootError::InvalidKernelMagic2);
    }

    Ok(())
}

/// Starts one validated Linux kernel image.
///
/// The kernel entry receives the standard RISC-V Linux boot arguments:
/// `a0 = boot_hart` and `a1 = updated_device_tree`.
///
/// # Parameters
///
/// - `kernel_image`: Loaded kernel image whose base address is jumped to.
/// - `boot_hart`: Hart identifier passed in register `a0`.
/// - `updated_device_tree`: Pointer to the updated DTB blob passed in `a1`.
pub unsafe fn start(
    kernel_image: &LoadedFile,
    boot_hart: usize,
    updated_device_tree: *const u8,
) -> ! {
    let kernel_entry = kernel_image.physical_start() as usize;

    crate::println!(
        "linux: start entry={:#018x}, boot_hart={}, device_tree={:#018x}",
        kernel_entry,
        boot_hart,
        updated_device_tree as usize,
    );

    unsafe {
        asm!(
            "fence.i",
            "jalr ra, 0({kernel_entry})",
            kernel_entry = in(reg) kernel_entry,
            in("a0") boot_hart,
            in("a1") updated_device_tree as usize,
            options(noreturn)
        );
    }
}

/// Parses the fixed-size Linux boot image header from `bytes`.
///
/// # Parameters
///
/// - `bytes`: Loaded kernel image bytes starting at the image base.
fn parse_linux_boot_image_header(
    bytes: &[u8],
) -> Result<LinuxBootImageHeader, LinuxBootError> {
    if bytes.len() < RISCV_LINUX_HEADER_SIZE {
        return Err(LinuxBootError::KernelImageTooSmall);
    }

    Ok(LinuxBootImageHeader {
        code0: read_le_u32(bytes, offset_of!(LinuxBootImageHeader, code0))
            .ok_or(LinuxBootError::KernelImageTooSmall)?,
        code1: read_le_u32(bytes, offset_of!(LinuxBootImageHeader, code1))
            .ok_or(LinuxBootError::KernelImageTooSmall)?,
        text_offset: read_le_u64(bytes, offset_of!(LinuxBootImageHeader, text_offset))
            .ok_or(LinuxBootError::KernelImageTooSmall)?,
        image_size: read_le_u64(bytes, offset_of!(LinuxBootImageHeader, image_size))
            .ok_or(LinuxBootError::KernelImageTooSmall)?,
        flags: read_le_u64(bytes, offset_of!(LinuxBootImageHeader, flags))
            .ok_or(LinuxBootError::KernelImageTooSmall)?,
        version: read_le_u32(bytes, offset_of!(LinuxBootImageHeader, version))
            .ok_or(LinuxBootError::KernelImageTooSmall)?,
        res1: read_le_u32(bytes, offset_of!(LinuxBootImageHeader, res1))
            .ok_or(LinuxBootError::KernelImageTooSmall)?,
        res2: read_le_u64(bytes, offset_of!(LinuxBootImageHeader, res2))
            .ok_or(LinuxBootError::KernelImageTooSmall)?,
        magic: read_le_u64(bytes, offset_of!(LinuxBootImageHeader, magic))
            .ok_or(LinuxBootError::KernelImageTooSmall)?,
        magic2: read_le_u32(bytes, offset_of!(LinuxBootImageHeader, magic2))
            .ok_or(LinuxBootError::KernelImageTooSmall)?,
        res3: read_le_u32(bytes, offset_of!(LinuxBootImageHeader, res3))
            .ok_or(LinuxBootError::KernelImageTooSmall)?,
    })
}

/// Dumps the first 0x40 bytes of the loaded kernel image to the debug console.
///
/// # Parameters
///
/// - `bytes`: Loaded kernel image bytes.
fn dump_kernel_header(bytes: &[u8]) {
    crate::println!("linux: first 0x40 kernel bytes");

    let limit = bytes.len().min(RISCV_LINUX_HEADER_SIZE);
    let mut index = 0usize;
    while index < limit {
        let mut column = 0usize;
        crate::print!("linux:   ");
        while column < 16 && index + column < limit {
            let value = bytes[index + column];
            crate::print!("{:02x} ", value);
            column += 1;
        }
        crate::println!("");
        index += 16;
    }
}

/// Reads one little-endian `u32` from `bytes`.
///
/// # Parameters
///
/// - `bytes`: Byte slice containing the encoded value.
/// - `offset`: Starting byte offset of the value.
fn read_le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let data = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

/// Reads one little-endian `u64` from `bytes`.
///
/// # Parameters
///
/// - `bytes`: Byte slice containing the encoded value.
/// - `offset`: Starting byte offset of the value.
fn read_le_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let data = bytes.get(offset..offset + 8)?;
    Some(u64::from_le_bytes([
        data[0], data[1], data[2], data[3],
        data[4], data[5], data[6], data[7],
    ]))
}