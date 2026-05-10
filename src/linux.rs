//! Linux boot-method request construction.
//!
//! This module validates the boot artifacts required for one Linux boot and
//! packages them into a Linux-specific request. The firmware loads the kernel
//! and optional initrd, clones and updates the device tree, validates the
//! RISC-V Linux boot header, and can then transfer control into the kernel.

use crate::dtb::{Dtb, DtbError};
use crate::ext4::Ext4Volume;
use crate::filesystem::{load_first_file, FileInfo, FileInfoView, FileSystem, FileType, LoadedFile};
use crate::fat::FatVolume;
use crate::memory::{AllocationDirection, EFI_MEMORY_TYPE, PageAllocator};
use crate::partition::PartitionEntry;
use crate::virtio::BlockDevice;
use core::arch::asm;
use core::mem::offset_of;
use core::ptr;
use core::str;

/// RISC-V Linux boot image header size in bytes.
const RISCV_LINUX_HEADER_SIZE: usize = 64;
/// Extra space reserved in the cloned Linux device tree.
const LINUX_DTB_EXTRA_SIZE: usize = 8 * 1024;
/// Deprecated RISC-V Linux boot header magic value, little-endian.
const RISCV_LINUX_MAGIC: u64 = 0x5643_5349_52;
/// Required RISC-V Linux boot header second magic value, little-endian.
const RISCV_LINUX_MAGIC2: u32 = 0x0543_5352;
/// RISC-V Linux kernels must be loaded at a 2 MiB-aligned address.
const KERNEL_ALIGNMENT: u64 = 2 * 1024 * 1024;

/// Fixed-capacity owned boot path stored without external pointers.
#[derive(Clone, Copy)]
struct BootPath {
    /// UTF-8 bytes for the path text.
    bytes: [u8; 16],
    /// Number of initialized bytes in `bytes`.
    len: usize,
}

impl BootPath {
    /// Builds one boot path by copying the provided byte slice.
    ///
    /// # Parameters
    ///
    /// - `bytes`: UTF-8 path bytes to store inline.
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > 16 {
            return None;
        }

        let mut path_bytes = [0u8; 16];
        path_bytes[..bytes.len()].copy_from_slice(bytes);
        Some(Self {
            bytes: path_bytes,
            len: bytes.len(),
        })
    }

    /// Returns the stored path as a UTF-8 string slice.
    fn as_str(&self) -> &str {
        unsafe { str::from_utf8_unchecked(&self.bytes[..self.len]) }
    }
}

impl AsRef<str> for BootPath {
    /// Returns the stored path as a string slice.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

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

/// Filesystem classification derived from probing one partition start sector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxBootFilesystem {
    /// The partition mounted successfully as FAT.
    Fat,
    /// The partition mounted successfully as ext4.
    Ext4,
    /// The partition did not match the supported filesystem probes.
    Unknown,
}

/// Returns one kernel candidate path by search order.
///
/// # Parameters
///
/// - `index`: Zero-based candidate index.
fn kernel_candidate_path(index: usize) -> Option<BootPath> {
    match index {
        0 => BootPath::from_bytes(b"/boot/vmlinuz"),
        1 => BootPath::from_bytes(b"/vmlinuz"),
        _ => None,
    }
}

/// Returns one initrd candidate path by search order.
///
/// # Parameters
///
/// - `index`: Zero-based candidate index.
fn initrd_candidate_path(index: usize) -> Option<BootPath> {
    match index {
        0 => BootPath::from_bytes(b"/boot/initrd.img"),
        1 => BootPath::from_bytes(b"/initrd.img"),
        _ => None,
    }
}

/// Tries the Linux boot method using one mounted filesystem and root partition.
///
/// # Parameters
///
/// - `volume`: Mounted filesystem chosen from the boot partition.
/// - `filesystem_name`: Filesystem label used in loaded-file logs.
/// - `allocator`: Live page allocator reused for artifact loads and DTB cloning.
/// - `block_device_index`: Zero-based virtio block-device index.
/// - `partition_number`: One-based partition number within the GPT.
/// - `boot_hart`: Original hart identifier received from OpenSBI.
/// - `device_tree_ptr`: Boot-time device-tree pointer received from OpenSBI.
pub fn try_boot_from_filesystem<F: FileSystem>(
    volume: &mut F,
    filesystem_name: &str,
    allocator: &mut PageAllocator<'_>,
    block_device_index: usize,
    partition_number: u32,
    boot_hart: usize,
    device_tree_ptr: *const u8,
) {
    let Some((kernel_path, kernel_loaded)) = load_first_file(
        volume,
        kernel_candidate_path,
        allocator,
        filesystem_name,
    ) else {
        return;
    };
    let initrd_loaded = load_first_file(
        volume,
        initrd_candidate_path,
        allocator,
        filesystem_name,
    );

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

    match check_kernel_header(&kernel_loaded) {
        Ok(()) => {
            crate::println!(
                "linux: kernel object {} matches RISC-V boot image header",
                kernel_path.as_str(),
            );
        }
        Err(_) => {
            crate::println!(
                "linux: kernel object {} does not match RISC-V boot image header",
                kernel_path.as_str(),
            );
            return;
        }
    }

    let kernel_loaded = match relocate_kernel(allocator, &kernel_loaded) {
        Some(kernel_loaded) => kernel_loaded,
        None => {
            crate::println!("linux: failed to relocate kernel to aligned memory");
            return;
        }
    };
    crate::println!(
        "linux: relocated kernel image to {:#018x}",
        kernel_loaded.physical_start() as usize,
    );

    let initrd_loaded = initrd_loaded.as_ref().map(|(path, loaded)| (path.as_str(), loaded));

    let _ = boot_and_start(
        &kernel_loaded,
        initrd_loaded,
        &device_tree,
        allocator,
        command_line,
        boot_hart,
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
pub fn try_boot_from_partition<D: BlockDevice, P: PartitionEntry>(
    device: &mut D,
    partition: P,
    filesystem: LinuxBootFilesystem,
    block_device_index: usize,
    partition_number: u32,
    boot_hart: usize,
    device_tree_ptr: *const u8,
) {
    match filesystem {
        LinuxBootFilesystem::Fat => {
            let mut volume = match FatVolume::new(device, partition.first_lba()) {
                Ok(volume) => volume,
                Err(_) => return,
            };
            try_boot_from_volume(
                &mut volume,
                "fat",
                block_device_index,
                partition_number,
                boot_hart,
                device_tree_ptr,
            );
        }
        LinuxBootFilesystem::Ext4 => {
            let mut volume = match Ext4Volume::new(device, partition.first_lba()) {
                Ok(volume) => volume,
                Err(_) => return,
            };
            try_boot_from_volume(
                &mut volume,
                "ext4",
                block_device_index,
                partition_number,
                boot_hart,
                device_tree_ptr,
            );
        }
        LinuxBootFilesystem::Unknown => {}
    }
}

/// Tries the Linux boot method using one mounted filesystem on a boot-flagged partition.
///
/// # Parameters
///
/// - `volume`: Mounted filesystem chosen from the boot-flagged partition.
/// - `filesystem_name`: Filesystem label used in loaded-file logs.
/// - `block_device_index`: Zero-based virtio block-device index.
/// - `partition_number`: One-based partition number within the GPT.
/// - `boot_hart`: Original hart identifier received from OpenSBI.
/// - `device_tree_ptr`: Boot-time device-tree pointer received from OpenSBI.
fn try_boot_from_volume<F: FileSystem>(
    volume: &mut F,
    filesystem_name: &str,
    block_device_index: usize,
    partition_number: u32,
    boot_hart: usize,
    device_tree_ptr: *const u8,
) {
    let mut regions = [crate::devicetree::MemoryRegion { base: 0, size: 0 }; 8];
    let mut reserved = [crate::devicetree::MemoryRegion { base: 0, size: 0 }; 16];
    let mut memory_map = [crate::EMPTY_MEMORY_DESCRIPTOR; 32];
    let mut allocator = match crate::page_allocator_from_live_fdt(
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

    try_boot_from_filesystem(
        volume,
        filesystem_name,
        &mut allocator,
        block_device_index,
        partition_number,
        boot_hart,
        device_tree_ptr,
    );
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

/// Builds a Linux boot request, updates `/chosen`, and transfers control to
/// the kernel.
///
/// # Parameters
///
/// - `kernel_image`: Loaded kernel image that will receive control.
/// - `initrd`: Optional initrd path plus loaded image used in the DTB.
/// - `device_tree`: Device tree object passed to the Linux boot flow.
/// - `allocator`: Page allocator used to clone the device tree for Linux.
/// - `command_line`: Linux kernel command line.
/// - `boot_hart`: Hart identifier passed in register `a0`.
pub fn boot_and_start<'a>(
    kernel_image: &LoadedFile,
    initrd: Option<(&str, &LoadedFile)>,
    device_tree: &Dtb,
    allocator: &mut PageAllocator<'_>,
    command_line: &'a str,
    boot_hart: usize,
) -> Result<(), LinuxBootError> {
    let kernel_info = FileInfo::new(FileType::File, kernel_image.size_bytes());
    let initrd_info = initrd.map(|(_, file)| {
        FileInfo::new(FileType::File, file.size_bytes())
    });

    let mut request = match boot(
        &kernel_info,
        initrd_info.as_ref(),
        device_tree,
        allocator,
        command_line,
    ) {
        Ok(request) => request,
        Err(error) => {
            crate::println!("linux: boot request rejected");
            return Err(error);
        }
    };

    if let Err(error) = request.update_device_tree(
        initrd.map(|(_, file)| file),
        command_line,
    ) {
        crate::println!("linux: failed to update cloned device-tree");
        return Err(error);
    }

    crate::println!("linux: transferring control to kernel");

    unsafe {
        start(kernel_image, boot_hart, request.device_tree().pointer());
    }
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
        "linux: entry={:#018x}, boot_hart={}, device_tree={:#018x}",
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

/// Copies one loaded kernel image into a low 2 MiB-aligned memory slot.
///
/// # Parameters
///
/// - `allocator`: Live page allocator reused for Linux boot allocations.
/// - `kernel_loaded`: Loaded kernel image currently stored in high memory.
fn relocate_kernel(
    allocator: &mut PageAllocator<'_>,
    kernel_loaded: &LoadedFile,
) -> Option<LoadedFile> {
    let relocated_start = allocator
        .allocate_aligned_pages_for_size(
            EFI_MEMORY_TYPE::EfiBootServicesData,
            kernel_loaded.size_bytes(),
            KERNEL_ALIGNMENT,
            AllocationDirection::Low,
        )
        .ok()?;

    unsafe {
        ptr::copy_nonoverlapping(
            kernel_loaded.physical_start() as *const u8,
            relocated_start as *mut u8,
            kernel_loaded.size_bytes(),
        );
    }

    allocator
        .FreePages(
            kernel_loaded.physical_start(),
            kernel_loaded.page_count(),
        )
        .ok()?;

    Some(LoadedFile::new(
        relocated_start,
        kernel_loaded.page_count(),
        kernel_loaded.size_bytes(),
    ))
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