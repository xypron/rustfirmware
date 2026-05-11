//! VirtIO MMIO support for the QEMU `virt` machine.
//!
//! This module currently provides three layers:
//! - MMIO transport discovery and register access
//! - virtqueue table layout and volatile access helpers
//! - a minimal polling block driver used to read sectors for GPT parsing

use core::mem;
use core::ptr::{self, NonNull};
use core::sync::atomic::{fence, Ordering};

/// VirtIO MMIO magic value for transport discovery.
pub const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
/// Version value used by modern VirtIO MMIO devices.
pub const VIRTIO_MMIO_VERSION_1: u32 = 2;
/// QEMU vendor identifier used by its VirtIO MMIO devices.
pub const VIRTIO_VENDOR_QEMU: u32 = 0x554d_4551;
/// VirtIO device identifier for block devices.
pub const VIRTIO_DEVICE_ID_BLOCK: u32 = 2;

/// Base address of the first VirtIO MMIO slot on the QEMU `virt` machine.
pub const VIRTIO_MMIO_QEMU_VIRT_BASE: usize = 0x1000_1000;
/// Distance between two adjacent VirtIO MMIO slots on the QEMU `virt` machine.
pub const VIRTIO_MMIO_QEMU_VIRT_STRIDE: usize = 0x1000;
/// Number of VirtIO MMIO slots exposed by the QEMU `virt` machine.
pub const VIRTIO_MMIO_QEMU_VIRT_COUNT: usize = 8;

/// Register offset of the transport magic value.
const MMIO_MAGIC_VALUE: usize = 0x000;
/// Register offset of the transport version field.
const MMIO_VERSION: usize = 0x004;
/// Register offset of the device identifier field.
const MMIO_DEVICE_ID: usize = 0x008;
/// Register offset of the vendor identifier field.
const MMIO_VENDOR_ID: usize = 0x00c;
/// Register offset of the device features register.
const MMIO_DEVICE_FEATURES: usize = 0x010;
/// Register offset of the device features selector register.
const MMIO_DEVICE_FEATURES_SEL: usize = 0x014;
/// Register offset of the driver features register.
const MMIO_DRIVER_FEATURES: usize = 0x020;
/// Register offset of the driver features selector register.
const MMIO_DRIVER_FEATURES_SEL: usize = 0x024;
/// Register offset of the queue selection register.
const MMIO_QUEUE_SEL: usize = 0x030;
/// Register offset of the maximum queue size register.
const MMIO_QUEUE_NUM_MAX: usize = 0x034;
/// Register offset of the negotiated queue size register.
const MMIO_QUEUE_NUM: usize = 0x038;
/// Register offset of the queue ready flag.
const MMIO_QUEUE_READY: usize = 0x044;
/// Register offset of the queue notification register.
const MMIO_QUEUE_NOTIFY: usize = 0x050;
/// Register offset of the interrupt status register.
const MMIO_INTERRUPT_STATUS: usize = 0x060;
/// Register offset of the interrupt acknowledge register.
const MMIO_INTERRUPT_ACK: usize = 0x064;
/// Register offset of the device status register.
const MMIO_STATUS: usize = 0x070;
/// Register offset of the low 32 bits of the descriptor table address.
const MMIO_QUEUE_DESC_LOW: usize = 0x080;
/// Register offset of the low 32 bits of the available ring address.
const MMIO_QUEUE_DRIVER_LOW: usize = 0x090;
/// Register offset of the low 32 bits of the used ring address.
const MMIO_QUEUE_DEVICE_LOW: usize = 0x0a0;
/// Register offset of the configuration generation counter.
const MMIO_CONFIG_GENERATION: usize = 0x0fc;

/// Driver status bit indicating that the guest acknowledged the device.
pub const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
/// Driver status bit indicating that a driver is present.
pub const VIRTIO_STATUS_DRIVER: u32 = 2;
/// Driver status bit indicating that the device is fully operational.
pub const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
/// Driver status bit indicating that feature negotiation completed.
pub const VIRTIO_STATUS_FEATURES_OK: u32 = 8;
/// Driver status bit indicating that the device requests a reset.
pub const VIRTIO_STATUS_DEVICE_NEEDS_RESET: u32 = 64;
/// Driver status bit indicating that the driver has failed.
pub const VIRTIO_STATUS_FAILED: u32 = 128;
/// Sector size used by the current VirtIO block driver.
pub const VIRTIO_SECTOR_SIZE: usize = 512;

/// Feature word selector containing `VIRTIO_F_VERSION_1`.
const VIRTIO_F_VERSION_1_SELECTOR: u32 = 1;
/// Bit mask for `VIRTIO_F_VERSION_1` within the selected feature word.
const VIRTIO_F_VERSION_1_BIT: u32 = 1;
/// VirtIO block request type for device-to-driver reads.
const VIRTIO_BLK_REQUEST_TYPE_IN: u32 = 0;
/// Successful completion status byte for a block request.
const VIRTIO_BLK_STATUS_OK: u8 = 0;
/// Queue number used for block requests.
const VIRTIO_BLOCK_QUEUE: u32 = 0;
/// Queue size provisioned for the polling block driver.
const VIRTIO_BLOCK_QUEUE_SIZE: u16 = 4;
/// Maximum completion-poll iterations before one block request times out.
const VIRTIO_BLOCK_REQUEST_POLL_LIMIT: usize = 10_000_000;

/// Virtqueue descriptor flag indicating a continuation descriptor.
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// Virtqueue descriptor flag indicating that the device writes into the buffer.
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
/// Virtqueue descriptor flag indicating an indirect descriptor table.
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// Avail ring flag suppressing interrupts from the device.
pub const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;
/// Used ring flag suppressing notifications to the device.
pub const VIRTQ_USED_F_NO_NOTIFY: u16 = 1;

/// Errors returned by the minimal VirtIO block driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtioError {
    /// The probed transport is not a VirtIO block device.
    NotBlockDevice,
    /// The selected queue is smaller than the driver requires.
    QueueTooSmall,
    /// The device did not advertise support for modern VirtIO version 1.
    MissingVersion1,
    /// The device rejected the negotiated driver feature set.
    FeatureNegotiationFailed,
    /// The in-memory virtqueue layout could not be constructed.
    QueueCreationFailed,
    /// The caller supplied a buffer whose length is not a whole number of sectors.
    InvalidBufferLength,
    /// The device did not complete a request before the polling limit expired.
    RequestTimeout,
    /// The device returned an unexpected descriptor head in the used ring.
    UnexpectedUsedId(u32),
    /// The device completed a block request with a non-zero status byte.
    BlockRequestFailed(u8),
}

/// Minimal block-device abstraction used by the GPT parser.
pub trait BlockDevice {
    /// Returns the total number of 512-byte sectors exposed by the device.
    fn sector_count(&self) -> u64;

    /// Reads one or more contiguous 512-byte sectors into `buffer`.
    ///
    /// # Parameters
    ///
    /// - `sector`: Logical block address of the first sector to read.
    /// - `buffer`: Destination buffer that receives the sector contents.
    ///   Its length must be a non-zero multiple of `VIRTIO_SECTOR_SIZE`.
    fn read_blocks(&mut self, sector: u64, buffer: &mut [u8]) -> Result<(), VirtioError>;
}

/// Header placed at the front of each VirtIO block request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
struct VirtioBlkReqHeader {
    /// VirtIO block request operation code.
    request_type: u32,
    /// Reserved field that must be zero.
    reserved: u32,
    /// Starting sector targeted by the request.
    sector: u64,
}

/// Single aligned allocation used to store the shared block request queue.
#[repr(align(4096))]
struct VirtioBlockQueueMemory(
    /// Raw queue storage sized for one 4 KiB page.
    [u8; 4096],
);

static mut VIRTIO_BLOCK_QUEUE_MEMORY: VirtioBlockQueueMemory = VirtioBlockQueueMemory([0; 4096]);
static mut VIRTIO_BLOCK_REQUEST_HEADER: VirtioBlkReqHeader = VirtioBlkReqHeader {
    request_type: 0,
    reserved: 0,
    sector: 0,
};
static mut VIRTIO_BLOCK_REQUEST_STATUS: u8 = 0xff;

/// Result of probing one QEMU `virt` MMIO slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtioMmioProbe {
    /// Zero-based QEMU `virt` MMIO slot index.
    pub slot: usize,
    /// Transport handle for the discovered device.
    pub device: VirtioMmioDevice,
}

/// Handle to one VirtIO MMIO transport instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtioMmioDevice {
    /// Base pointer for the mapped VirtIO MMIO register block.
    base: NonNull<u8>,
}

/// Iterator over block devices exposed on the fixed QEMU `virt` MMIO slots.
pub struct QemuVirtBlockDevices {
    /// Next MMIO slot index to probe.
    slot: usize,
}

/// Minimal polling VirtIO block driver backed by queue `0`.
pub struct VirtioBlockDriver {
    /// Transport handle for the underlying MMIO device.
    device: VirtioMmioDevice,
    /// Total number of 512-byte sectors reported by the device.
    capacity_sectors: u64,
    /// Guest-memory view of the configured request queue.
    queue: VirtQueue,
    /// Next available-ring index to publish.
    avail_idx: u16,
    /// Last used-ring index consumed by the driver.
    used_idx: u16,
}

impl VirtioMmioDevice {
    /// Creates an MMIO transport handle from a raw base address.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `base` points at a valid, mapped VirtIO
    /// MMIO register region with the expected alignment and lifetime, and that
    /// no conflicting aliasing transport handle is used to mutate the same
    /// device concurrently.
    ///
    /// # Parameters
    ///
    /// - `base`: Physical MMIO base address of the VirtIO transport.
    pub unsafe fn from_base(base: usize) -> Option<Self> {
        Some(Self {
            base: NonNull::new(base as *mut u8)?,
        })
    }

    /// Returns the MMIO base address for one QEMU `virt` VirtIO slot.
    ///
    /// # Parameters
    ///
    /// - `index`: Zero-based MMIO slot number on the QEMU `virt` machine.
    pub const fn qemu_virt_base(index: usize) -> usize {
        VIRTIO_MMIO_QEMU_VIRT_BASE + (index * VIRTIO_MMIO_QEMU_VIRT_STRIDE)
    }

    /// Returns the raw MMIO base address of the device.
    pub fn base_address(&self) -> usize {
        self.base.as_ptr() as usize
    }

    /// Reads the transport magic value.
    pub fn magic_value(&self) -> u32 {
        self.read_reg(MMIO_MAGIC_VALUE)
    }

    /// Reads the transport version.
    pub fn version(&self) -> u32 {
        self.read_reg(MMIO_VERSION)
    }

    /// Reads the VirtIO device identifier.
    pub fn device_id(&self) -> u32 {
        self.read_reg(MMIO_DEVICE_ID)
    }

    /// Reads the device vendor identifier.
    pub fn vendor_id(&self) -> u32 {
        self.read_reg(MMIO_VENDOR_ID)
    }

    /// Returns `true` when the MMIO slot contains a valid VirtIO device.
    pub fn is_present(&self) -> bool {
        self.magic_value() == VIRTIO_MMIO_MAGIC
            && self.version() == VIRTIO_MMIO_VERSION_1
            && self.device_id() != 0
    }

    /// Returns `true` when the transport hosts a VirtIO block device.
    pub fn is_block_device(&self) -> bool {
        self.is_present() && self.device_id() == VIRTIO_DEVICE_ID_BLOCK
    }

    /// Reads one 32-bit page of device features.
    ///
    /// # Parameters
    ///
    /// - `selector`: Feature page selector written before reading the register.
    pub fn device_features(&self, selector: u32) -> u32 {
        self.write_reg(MMIO_DEVICE_FEATURES_SEL, selector);
        self.read_reg(MMIO_DEVICE_FEATURES)
    }

    /// Writes one 32-bit page of negotiated driver features.
    ///
    /// # Parameters
    ///
    /// - `selector`: Feature page selector to update.
    /// - `features`: Bitmask of negotiated features for that page.
    pub fn set_driver_features(&mut self, selector: u32, features: u32) {
        self.write_reg(MMIO_DRIVER_FEATURES_SEL, selector);
        self.write_reg(MMIO_DRIVER_FEATURES, features);
    }

    /// Selects the active virtqueue register bank.
    ///
    /// # Parameters
    ///
    /// - `queue_index`: Queue number whose registers should become active.
    pub fn select_queue(&mut self, queue_index: u32) {
        self.write_reg(MMIO_QUEUE_SEL, queue_index)
    }

    /// Returns the maximum queue size supported by the selected queue.
    pub fn queue_num_max(&self) -> u32 {
        self.read_reg(MMIO_QUEUE_NUM_MAX)
    }

    /// Sets the queue size for the selected queue.
    ///
    /// # Parameters
    ///
    /// - `queue_size`: Queue size to program into the selected queue.
    pub fn set_queue_num(&mut self, queue_size: u32) {
        self.write_reg(MMIO_QUEUE_NUM, queue_size)
    }

    /// Returns the ready flag for the selected queue.
    pub fn queue_ready(&self) -> u32 {
        self.read_reg(MMIO_QUEUE_READY)
    }

    /// Marks the selected queue as ready or not ready.
    ///
    /// # Parameters
    ///
    /// - `ready`: Whether the selected queue should be marked ready.
    pub fn set_queue_ready(&mut self, ready: bool) {
        self.write_reg(MMIO_QUEUE_READY, ready as u32)
    }

    /// Programs the descriptor, driver, and device ring addresses.
    ///
    /// # Parameters
    ///
    /// - `desc`: Physical address of the descriptor table.
    /// - `driver`: Physical address of the available ring.
    /// - `device`: Physical address of the used ring.
    pub fn set_queue_addresses(&mut self, desc: u64, driver: u64, device: u64) {
        self.write_reg64(MMIO_QUEUE_DESC_LOW, desc);
        self.write_reg64(MMIO_QUEUE_DRIVER_LOW, driver);
        self.write_reg64(MMIO_QUEUE_DEVICE_LOW, device);
    }

    /// Notifies the device that work is available on a queue.
    ///
    /// # Parameters
    ///
    /// - `queue_index`: Queue number to notify.
    pub fn notify_queue(&mut self, queue_index: u32) {
        self.write_reg(MMIO_QUEUE_NOTIFY, queue_index)
    }

    /// Reads the current interrupt status bits.
    pub fn interrupt_status(&self) -> u32 {
        self.read_reg(MMIO_INTERRUPT_STATUS)
    }

    /// Acknowledges pending interrupt status bits.
    ///
    /// # Parameters
    ///
    /// - `bits`: Interrupt status bits to acknowledge.
    pub fn acknowledge_interrupt(&mut self, bits: u32) {
        self.write_reg(MMIO_INTERRUPT_ACK, bits)
    }

    /// Reads the device status register.
    pub fn status(&self) -> u32 {
        self.read_reg(MMIO_STATUS)
    }

    /// Resets the device status register.
    pub fn reset(&mut self) {
        self.write_reg(MMIO_STATUS, 0)
    }

    /// Sets one or more device status bits.
    ///
    /// # Parameters
    ///
    /// - `status`: Status bits to OR into the current device status register.
    pub fn add_status(&mut self, status: u32) {
        self.write_reg(MMIO_STATUS, self.status() | status)
    }

    /// Reads the configuration generation counter.
    pub fn config_generation(&self) -> u32 {
        self.read_reg(MMIO_CONFIG_GENERATION)
    }

    /// Reads one little-endian `u64` from the device-specific configuration area.
    ///
    /// # Parameters
    ///
    /// - `offset`: Byte offset within the device-specific configuration area.
    pub fn read_config_u64_le(&self, offset: usize) -> u64 {
        loop {
            let before = self.config_generation();
            let mut bytes = [0u8; 8];

            let mut index = 0usize;
            while index < bytes.len() {
                bytes[index] = unsafe {
                    ptr::read_volatile(self.base.as_ptr().add(0x100 + offset + index))
                };
                index += 1;
            }

            if before == self.config_generation() {
                return u64::from_le_bytes(bytes);
            }
        }
    }

    fn read_reg(&self, offset: usize) -> u32 {
        // SAFETY: `self.base` was constructed from a valid VirtIO MMIO region,
        // and all callers use register offsets within that mapped register
        // block.
        unsafe { ptr::read_volatile(self.register_ptr(offset)) }
    }

    fn write_reg(&self, offset: usize, value: u32) {
        // SAFETY: `self.base` was constructed from a valid VirtIO MMIO region,
        // and all callers use register offsets within that mapped register
        // block.
        unsafe { ptr::write_volatile(self.register_mut_ptr(offset), value) }
    }

    fn write_reg64(&self, offset_low: usize, value: u64) {
        self.write_reg(offset_low, value as u32);
        self.write_reg(offset_low + 4, (value >> 32) as u32);
    }

    fn register_ptr(&self, offset: usize) -> *const u32 {
        unsafe { self.base.as_ptr().add(offset) as *const u32 }
    }

    fn register_mut_ptr(&self, offset: usize) -> *mut u32 {
        self.register_ptr(offset) as *mut u32
    }
}

impl QemuVirtBlockDevices {
    const fn new() -> Self {
        Self { slot: 0 }
    }
}

impl Iterator for QemuVirtBlockDevices {
    type Item = VirtioMmioProbe;

    fn next(&mut self) -> Option<Self::Item> {
        while self.slot < VIRTIO_MMIO_QEMU_VIRT_COUNT {
            let slot = self.slot;
            self.slot += 1;

            let device = unsafe { VirtioMmioDevice::from_base(VirtioMmioDevice::qemu_virt_base(slot)) }?;

            if device.is_block_device() {
                return Some(VirtioMmioProbe { slot, device });
            }
        }

        None
    }
}

/// Returns an iterator over all block devices on the fixed QEMU MMIO slots.
pub fn qemu_virt_block_devices() -> QemuVirtBlockDevices {
    QemuVirtBlockDevices::new()
}

/// Probes the QEMU MMIO slots and returns the first block device, if present.
///
/// # Safety
///
/// The caller must ensure that probing fixed QEMU MMIO addresses is valid in
/// the current machine configuration and that the returned transport handle is
/// not used concurrently with other mutable access paths.
pub unsafe fn probe_qemu_virt_block_device() -> Option<VirtioMmioDevice> {
    qemu_virt_block_devices().next().map(|probe| probe.device)
}

impl VirtioBlockDriver {
    /// Initializes a minimal polling block driver for a VirtIO MMIO block device.
    ///
    /// # Safety
    ///
    /// The caller must ensure that only one instance uses the shared static
    /// queue/request storage at a time. This firmware only runs one hart
    /// through the block-driver path, so the shared statics remain single-
    /// threaded as long as callers uphold that one-driver-at-a-time contract.
    ///
    /// # Parameters
    ///
    /// - `device`: Probed VirtIO MMIO transport configured as a block device.
    pub unsafe fn new(mut device: VirtioMmioDevice) -> Result<Self, VirtioError> {
        if !device.is_block_device() {
            return Err(VirtioError::NotBlockDevice);
        }

        let capacity_sectors = device.read_config_u64_le(0);

        device.reset();
        device.add_status(VIRTIO_STATUS_ACKNOWLEDGE);
        device.add_status(VIRTIO_STATUS_DRIVER);

        let device_features_hi = device.device_features(VIRTIO_F_VERSION_1_SELECTOR);
        if (device_features_hi & VIRTIO_F_VERSION_1_BIT) == 0 {
            return Err(VirtioError::MissingVersion1);
        }

        device.set_driver_features(0, 0);
        device.set_driver_features(VIRTIO_F_VERSION_1_SELECTOR, VIRTIO_F_VERSION_1_BIT);
        device.add_status(VIRTIO_STATUS_FEATURES_OK);

        if (device.status() & VIRTIO_STATUS_FEATURES_OK) == 0 {
            return Err(VirtioError::FeatureNegotiationFailed);
        }

        device.select_queue(VIRTIO_BLOCK_QUEUE);
        if device.queue_num_max() < VIRTIO_BLOCK_QUEUE_SIZE as u32 {
            return Err(VirtioError::QueueTooSmall);
        }

        let queue_layout = VirtQueue::layout(VIRTIO_BLOCK_QUEUE_SIZE, false);
        let queue_base = unsafe { ptr::addr_of_mut!(VIRTIO_BLOCK_QUEUE_MEMORY.0) as *mut u8 };
        if queue_layout.total_size > 4096 {
            return Err(VirtioError::QueueTooSmall);
        }

        // SAFETY: `queue_base` points at the dedicated shared 4 KiB queue page,
        // which is valid and uniquely used by this driver instance.
        unsafe {
            ptr::write_bytes(queue_base, 0, 4096);
        }

        // SAFETY: `queue_base` points at writable queue memory large enough for
        // `queue_layout`, validated against the dedicated 4 KiB backing page.
        let queue = unsafe { VirtQueue::from_ptr(queue_base, VIRTIO_BLOCK_QUEUE_SIZE, false) }
            .ok_or(VirtioError::QueueCreationFailed)?;

        device.set_queue_num(VIRTIO_BLOCK_QUEUE_SIZE as u32);
        device.set_queue_addresses(
            queue_base as u64 + queue_layout.desc_offset as u64,
            queue_base as u64 + queue_layout.avail_offset as u64,
            queue_base as u64 + queue_layout.used_offset as u64,
        );
        device.set_queue_ready(true);
        device.add_status(VIRTIO_STATUS_DRIVER_OK);

        Ok(Self {
            device,
            capacity_sectors,
            queue,
            avail_idx: 0,
            used_idx: 0,
        })
    }
}

impl BlockDevice for VirtioBlockDriver {
    fn sector_count(&self) -> u64 {
        self.capacity_sectors
    }

    fn read_blocks(&mut self, sector: u64, buffer: &mut [u8]) -> Result<(), VirtioError> {
        if buffer.is_empty() || (buffer.len() % VIRTIO_SECTOR_SIZE) != 0 {
            return Err(VirtioError::InvalidBufferLength);
        }

        let buffer_len = u32::try_from(buffer.len()).map_err(|_| VirtioError::InvalidBufferLength)?;

        // SAFETY: The shared request header/status storage is exclusively owned
        // by this single active driver instance while the request is in flight.
        unsafe {
            ptr::write(
                ptr::addr_of_mut!(VIRTIO_BLOCK_REQUEST_HEADER),
                VirtioBlkReqHeader {
                    request_type: VIRTIO_BLK_REQUEST_TYPE_IN,
                    reserved: 0,
                    sector,
                },
            );
            ptr::write_volatile(ptr::addr_of_mut!(VIRTIO_BLOCK_REQUEST_STATUS), 0xff);
        }

        self.queue.write_descriptor(
            0,
            VirtqDescriptor {
                addr: ptr::addr_of!(VIRTIO_BLOCK_REQUEST_HEADER) as u64,
                len: mem::size_of::<VirtioBlkReqHeader>() as u32,
                flags: VIRTQ_DESC_F_NEXT,
                next: 1,
            },
        );
        self.queue.write_descriptor(
            1,
            VirtqDescriptor {
                addr: buffer.as_mut_ptr() as u64,
                len: buffer_len,
                flags: VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT,
                next: 2,
            },
        );
        self.queue.write_descriptor(
            2,
            VirtqDescriptor {
                addr: ptr::addr_of!(VIRTIO_BLOCK_REQUEST_STATUS) as u64,
                len: 1,
                flags: VIRTQ_DESC_F_WRITE,
                next: 0,
            },
        );

        let ring_slot = self.avail_idx % VIRTIO_BLOCK_QUEUE_SIZE;
        self.queue.set_avail_ring(ring_slot, 0);
        fence(Ordering::SeqCst);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.queue.set_avail_idx(self.avail_idx);
        fence(Ordering::SeqCst);
        self.device.notify_queue(VIRTIO_BLOCK_QUEUE);

        let mut spins = 0usize;
        while self.queue.used_idx() == self.used_idx {
            if spins == VIRTIO_BLOCK_REQUEST_POLL_LIMIT {
                return Err(VirtioError::RequestTimeout);
            }
            spins += 1;
            core::hint::spin_loop();
        }

        fence(Ordering::SeqCst);

        let used_slot = self.used_idx % VIRTIO_BLOCK_QUEUE_SIZE;
        let used_elem = self.queue.used_elem(used_slot);
        if used_elem.id != 0 {
            return Err(VirtioError::UnexpectedUsedId(used_elem.id));
        }

        self.used_idx = self.used_idx.wrapping_add(1);

        let status = unsafe { ptr::read_volatile(ptr::addr_of!(VIRTIO_BLOCK_REQUEST_STATUS)) };
        if status == VIRTIO_BLK_STATUS_OK {
            Ok(())
        } else {
            Err(VirtioError::BlockRequestFailed(status))
        }
    }
}

/// One virtqueue descriptor entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct VirtqDescriptor {
    /// Guest-physical address of the buffer described by this entry.
    pub addr: u64,
    /// Length in bytes of the described buffer.
    pub len: u32,
    /// Descriptor flags such as NEXT or WRITE.
    pub flags: u16,
    /// Index of the next descriptor when `VIRTQ_DESC_F_NEXT` is set.
    pub next: u16,
}

/// One element written by the device into the used ring.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct VirtqUsedElem {
    /// Head descriptor index returned by the device.
    pub id: u32,
    /// Total number of bytes the device reported as written.
    pub len: u32,
}

/// Computed layout of a packed virtqueue memory block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtQueueLayout {
    /// Number of descriptors in the queue.
    pub queue_size: u16,
    /// Whether event index fields are included in the rings.
    pub event_idx: bool,
    /// Byte offset of the descriptor table from the queue base.
    pub desc_offset: usize,
    /// Byte offset of the available ring from the queue base.
    pub avail_offset: usize,
    /// Byte offset of the used ring from the queue base.
    pub used_offset: usize,
    /// Total queue memory size in bytes, including alignment padding.
    pub total_size: usize,
}

impl VirtQueueLayout {
    /// Computes the in-memory layout of a queue.
    ///
    /// # Parameters
    ///
    /// - `queue_size`: Number of descriptors in the queue.
    /// - `event_idx`: Whether event index fields should be included.
    pub const fn new(queue_size: u16, event_idx: bool) -> Self {
        // This helper is only used with small VirtIO queue sizes. The current
        // block driver provisions four descriptors, well below any arithmetic
        // overflow boundary for these layout calculations.
        let desc_offset = 0;
        let desc_size = mem::size_of::<VirtqDescriptor>() * queue_size as usize;
        let avail_offset = desc_offset + desc_size;
        let avail_size = 4 + (queue_size as usize * 2) + if event_idx { 2 } else { 0 };
        let used_offset = align_up(avail_offset + avail_size, 4);
        let used_size = 4 + (queue_size as usize * mem::size_of::<VirtqUsedElem>()) + if event_idx { 2 } else { 0 };

        Self {
            queue_size,
            event_idx,
            desc_offset,
            avail_offset,
            used_offset,
            total_size: used_offset + used_size,
        }
    }
}

/// Volatile accessor for one virtqueue stored in guest memory.
pub struct VirtQueue {
    /// Base pointer of the queue memory block.
    base: NonNull<u8>,
    /// Precomputed offsets and size information for this queue.
    layout: VirtQueueLayout,
}

impl VirtQueue {
    /// Computes the layout needed for a queue with the given parameters.
    pub const fn layout(queue_size: u16, event_idx: bool) -> VirtQueueLayout {
        VirtQueueLayout::new(queue_size, event_idx)
    }

    /// Creates a queue accessor from a raw guest-memory pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `base` points at writable guest memory large
    /// enough for the requested queue layout, with alignment suitable for the
    /// descriptor table and ring structures derived from it.
    pub unsafe fn from_ptr(base: *mut u8, queue_size: u16, event_idx: bool) -> Option<Self> {
        Some(Self {
            base: NonNull::new(base)?,
            layout: VirtQueueLayout::new(queue_size, event_idx),
        })
    }

    /// Returns the configured queue size.
    pub const fn queue_size(&self) -> u16 {
        self.layout.queue_size
    }

    /// Returns whether EVENT_IDX fields are present.
    pub const fn event_idx(&self) -> bool {
        self.layout.event_idx
    }

    /// Returns the total number of bytes required by the queue layout.
    pub const fn total_size(&self) -> usize {
        self.layout.total_size
    }

    /// Reads one descriptor entry from the descriptor table.
    ///
    /// # Parameters
    ///
    /// - `index`: Descriptor slot index within the queue.
    pub fn read_descriptor(&self, index: u16) -> VirtqDescriptor {
        debug_assert!(index < self.queue_size());
        unsafe { ptr::read_volatile(self.descriptor_ptr(index)) }
    }

    /// Writes one descriptor entry into the descriptor table.
    ///
    /// # Parameters
    ///
    /// - `index`: Descriptor slot index within the queue.
    /// - `descriptor`: Descriptor value written to the selected slot.
    pub fn write_descriptor(&mut self, index: u16, descriptor: VirtqDescriptor) {
        debug_assert!(index < self.queue_size());
        unsafe { ptr::write_volatile(self.descriptor_mut_ptr(index), descriptor) }
    }

    /// Returns the available-ring flags field.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    pub fn avail_flags(&self) -> u16 {
        unsafe { ptr::read_volatile(self.avail_flags_ptr()) }
    }

    /// Writes the available-ring flags field.
    ///
    /// # Parameters
    ///
    /// - `flags`: New value for the available-ring flags field.
    pub fn set_avail_flags(&mut self, flags: u16) {
        unsafe { ptr::write_volatile(self.avail_flags_mut_ptr(), flags) }
    }

    /// Returns the available-ring index field.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    pub fn avail_idx(&self) -> u16 {
        unsafe { ptr::read_volatile(self.avail_idx_ptr()) }
    }

    /// Writes the available-ring index field.
    ///
    /// # Parameters
    ///
    /// - `index`: New producer index for the available ring.
    pub fn set_avail_idx(&mut self, index: u16) {
        unsafe { ptr::write_volatile(self.avail_idx_mut_ptr(), index) }
    }

    /// Returns one available-ring entry.
    ///
    /// # Parameters
    ///
    /// - `slot`: Ring slot to read.
    pub fn avail_ring(&self, slot: u16) -> u16 {
        debug_assert!(slot < self.queue_size());
        unsafe { ptr::read_volatile(self.avail_ring_ptr(slot)) }
    }

    /// Writes one available-ring entry.
    ///
    /// # Parameters
    ///
    /// - `slot`: Ring slot to update.
    /// - `descriptor_index`: Descriptor head index published in that slot.
    pub fn set_avail_ring(&mut self, slot: u16, descriptor_index: u16) {
        debug_assert!(slot < self.queue_size());
        unsafe { ptr::write_volatile(self.avail_ring_mut_ptr(slot), descriptor_index) }
    }

    /// Returns the used-ring flags field.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    pub fn used_flags(&self) -> u16 {
        unsafe { ptr::read_volatile(self.used_flags_ptr()) }
    }

    /// Writes the used-ring flags field.
    ///
    /// # Parameters
    ///
    /// - `flags`: New value for the used-ring flags field.
    pub fn set_used_flags(&mut self, flags: u16) {
        unsafe { ptr::write_volatile(self.used_flags_mut_ptr(), flags) }
    }

    /// Returns the used-ring index field.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    pub fn used_idx(&self) -> u16 {
        unsafe { ptr::read_volatile(self.used_idx_ptr()) }
    }

    /// Writes the used-ring index field.
    ///
    /// # Parameters
    ///
    /// - `index`: New consumer index for the used ring.
    pub fn set_used_idx(&mut self, index: u16) {
        unsafe { ptr::write_volatile(self.used_idx_mut_ptr(), index) }
    }

    /// Returns one used-ring element.
    ///
    /// # Parameters
    ///
    /// - `slot`: Ring slot to read.
    pub fn used_elem(&self, slot: u16) -> VirtqUsedElem {
        debug_assert!(slot < self.queue_size());
        unsafe { ptr::read_volatile(self.used_ring_ptr(slot)) }
    }

    /// Writes one used-ring element.
    ///
    /// # Parameters
    ///
    /// - `slot`: Ring slot to update.
    /// - `elem`: Used-ring element written into the selected slot.
    pub fn set_used_elem(&mut self, slot: u16, elem: VirtqUsedElem) {
        debug_assert!(slot < self.queue_size());
        unsafe { ptr::write_volatile(self.used_ring_mut_ptr(slot), elem) }
    }

    /// Returns the optional used-event field when EVENT_IDX is enabled.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    pub fn used_event(&self) -> Option<u16> {
        if !self.event_idx() {
            return None;
        }

        Some(unsafe { ptr::read_volatile(self.used_event_ptr()) })
    }

    /// Writes the used-event field when EVENT_IDX is enabled.
    ///
    /// # Parameters
    ///
    /// - `event`: Event threshold stored in the used-event field.
    pub fn set_used_event(&mut self, event: u16) {
        debug_assert!(self.event_idx());
        unsafe { ptr::write_volatile(self.used_event_mut_ptr(), event) }
    }

    /// Returns the optional available-event field when EVENT_IDX is enabled.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    pub fn avail_event(&self) -> Option<u16> {
        if !self.event_idx() {
            return None;
        }

        Some(unsafe { ptr::read_volatile(self.avail_event_ptr()) })
    }

    /// Writes the available-event field when EVENT_IDX is enabled.
    ///
    /// # Parameters
    ///
    /// - `event`: Event threshold stored in the available-event field.
    pub fn set_avail_event(&mut self, event: u16) {
        debug_assert!(self.event_idx());
        unsafe { ptr::write_volatile(self.avail_event_mut_ptr(), event) }
    }

    fn descriptor_ptr(&self, index: u16) -> *const VirtqDescriptor {
        // SAFETY: Debug assertions constrain `index` to the configured queue
        // size, and `self.base` points at queue memory large enough for the
        // precomputed layout.
        unsafe { self.base.as_ptr().add(self.layout.desc_offset + index as usize * mem::size_of::<VirtqDescriptor>()) as *const VirtqDescriptor }
    }

    fn descriptor_mut_ptr(&mut self, index: u16) -> *mut VirtqDescriptor {
        self.descriptor_ptr(index) as *mut VirtqDescriptor
    }

    fn avail_flags_ptr(&self) -> *const u16 {
        // SAFETY: `self.base` points at queue memory large enough for the
        // precomputed layout, including the available ring header.
        unsafe { self.base.as_ptr().add(self.layout.avail_offset) as *const u16 }
    }

    fn avail_flags_mut_ptr(&mut self) -> *mut u16 {
        self.avail_flags_ptr() as *mut u16
    }

    fn avail_idx_ptr(&self) -> *const u16 {
        // SAFETY: `self.base` points at queue memory large enough for the
        // precomputed layout, including the available ring header.
        unsafe { self.base.as_ptr().add(self.layout.avail_offset + 2) as *const u16 }
    }

    fn avail_idx_mut_ptr(&mut self) -> *mut u16 {
        self.avail_idx_ptr() as *mut u16
    }

    fn avail_ring_ptr(&self, slot: u16) -> *const u16 {
        // SAFETY: Debug assertions constrain `slot` to the queue size, and the
        // precomputed layout reserves space for every available-ring entry.
        unsafe { self.base.as_ptr().add(self.layout.avail_offset + 4 + slot as usize * 2) as *const u16 }
    }

    fn avail_ring_mut_ptr(&mut self, slot: u16) -> *mut u16 {
        self.avail_ring_ptr(slot) as *mut u16
    }

    fn used_flags_ptr(&self) -> *const u16 {
        // SAFETY: `self.base` points at queue memory large enough for the
        // precomputed layout, including the used ring header.
        unsafe { self.base.as_ptr().add(self.layout.used_offset) as *const u16 }
    }

    fn used_flags_mut_ptr(&mut self) -> *mut u16 {
        self.used_flags_ptr() as *mut u16
    }

    fn used_idx_ptr(&self) -> *const u16 {
        // SAFETY: `self.base` points at queue memory large enough for the
        // precomputed layout, including the used ring header.
        unsafe { self.base.as_ptr().add(self.layout.used_offset + 2) as *const u16 }
    }

    fn used_idx_mut_ptr(&mut self) -> *mut u16 {
        self.used_idx_ptr() as *mut u16
    }

    fn used_ring_ptr(&self, slot: u16) -> *const VirtqUsedElem {
        // SAFETY: Debug assertions constrain `slot` to the queue size, and the
        // precomputed layout reserves space for every used-ring entry.
        unsafe {
            self.base.as_ptr().add(self.layout.used_offset + 4 + slot as usize * mem::size_of::<VirtqUsedElem>())
                as *const VirtqUsedElem
        }
    }

    fn used_ring_mut_ptr(&mut self, slot: u16) -> *mut VirtqUsedElem {
        self.used_ring_ptr(slot) as *mut VirtqUsedElem
    }

    fn used_event_ptr(&self) -> *const u16 {
        // SAFETY: This field is only used when EVENT_IDX is enabled, and the
        // precomputed layout includes its trailing available-ring storage.
        unsafe { self.base.as_ptr().add(self.layout.avail_offset + 4 + self.queue_size() as usize * 2) as *const u16 }
    }

    fn used_event_mut_ptr(&mut self) -> *mut u16 {
        self.used_event_ptr() as *mut u16
    }

    fn avail_event_ptr(&self) -> *const u16 {
        // SAFETY: This field is only used when EVENT_IDX is enabled, and the
        // precomputed layout includes its trailing used-ring storage.
        unsafe {
            self.base.as_ptr().add(
                self.layout.used_offset + 4 + self.queue_size() as usize * mem::size_of::<VirtqUsedElem>(),
            ) as *const u16
        }
    }

    fn avail_event_mut_ptr(&mut self) -> *mut u16 {
        self.avail_event_ptr() as *mut u16
    }
}

const fn align_up(value: usize, align: usize) -> usize {
    (value + (align - 1)) & !(align - 1)
}