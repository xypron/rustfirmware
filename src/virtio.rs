use core::mem;
use core::ptr::{self, NonNull};
use core::sync::atomic::{fence, Ordering};

pub const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
pub const VIRTIO_MMIO_VERSION_1: u32 = 2;
pub const VIRTIO_VENDOR_QEMU: u32 = 0x554d_4551;
pub const VIRTIO_DEVICE_ID_BLOCK: u32 = 2;

pub const VIRTIO_MMIO_QEMU_VIRT_BASE: usize = 0x1000_1000;
pub const VIRTIO_MMIO_QEMU_VIRT_STRIDE: usize = 0x1000;
pub const VIRTIO_MMIO_QEMU_VIRT_COUNT: usize = 8;

const MMIO_MAGIC_VALUE: usize = 0x000;
const MMIO_VERSION: usize = 0x004;
const MMIO_DEVICE_ID: usize = 0x008;
const MMIO_VENDOR_ID: usize = 0x00c;
const MMIO_DEVICE_FEATURES: usize = 0x010;
const MMIO_DEVICE_FEATURES_SEL: usize = 0x014;
const MMIO_DRIVER_FEATURES: usize = 0x020;
const MMIO_DRIVER_FEATURES_SEL: usize = 0x024;
const MMIO_QUEUE_SEL: usize = 0x030;
const MMIO_QUEUE_NUM_MAX: usize = 0x034;
const MMIO_QUEUE_NUM: usize = 0x038;
const MMIO_QUEUE_READY: usize = 0x044;
const MMIO_QUEUE_NOTIFY: usize = 0x050;
const MMIO_INTERRUPT_STATUS: usize = 0x060;
const MMIO_INTERRUPT_ACK: usize = 0x064;
const MMIO_STATUS: usize = 0x070;
const MMIO_QUEUE_DESC_LOW: usize = 0x080;
const MMIO_QUEUE_DRIVER_LOW: usize = 0x090;
const MMIO_QUEUE_DEVICE_LOW: usize = 0x0a0;
const MMIO_CONFIG_GENERATION: usize = 0x0fc;

pub const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
pub const VIRTIO_STATUS_DRIVER: u32 = 2;
pub const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
pub const VIRTIO_STATUS_FEATURES_OK: u32 = 8;
pub const VIRTIO_STATUS_DEVICE_NEEDS_RESET: u32 = 64;
pub const VIRTIO_STATUS_FAILED: u32 = 128;
pub const VIRTIO_SECTOR_SIZE: usize = 512;

const VIRTIO_F_VERSION_1_SELECTOR: u32 = 1;
const VIRTIO_F_VERSION_1_BIT: u32 = 1;
const VIRTIO_BLK_REQUEST_TYPE_IN: u32 = 0;
const VIRTIO_BLK_STATUS_OK: u8 = 0;
const VIRTIO_BLOCK_QUEUE: u32 = 0;
const VIRTIO_BLOCK_QUEUE_SIZE: u16 = 4;

pub const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

pub const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;
pub const VIRTQ_USED_F_NO_NOTIFY: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtioError {
    NotBlockDevice,
    QueueTooSmall,
    MissingVersion1,
    FeatureNegotiationFailed,
    QueueCreationFailed,
    BlockRequestFailed(u8),
}

pub trait BlockDevice {
    fn read_sector(&mut self, sector: u64, buffer: &mut [u8; VIRTIO_SECTOR_SIZE]) -> Result<(), VirtioError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
struct VirtioBlkReqHeader {
    request_type: u32,
    reserved: u32,
    sector: u64,
}

#[repr(align(4096))]
struct VirtioBlockQueueMemory([u8; 4096]);

static mut VIRTIO_BLOCK_QUEUE_MEMORY: VirtioBlockQueueMemory = VirtioBlockQueueMemory([0; 4096]);
static mut VIRTIO_BLOCK_REQUEST_HEADER: VirtioBlkReqHeader = VirtioBlkReqHeader {
    request_type: 0,
    reserved: 0,
    sector: 0,
};
static mut VIRTIO_BLOCK_REQUEST_STATUS: u8 = 0xff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtioMmioProbe {
    pub slot: usize,
    pub device: VirtioMmioDevice,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtioMmioDevice {
    base: NonNull<u8>,
}

pub struct QemuVirtBlockDevices {
    slot: usize,
}

pub struct VirtioBlockDriver {
    device: VirtioMmioDevice,
    queue: VirtQueue,
    avail_idx: u16,
    used_idx: u16,
}

impl VirtioMmioDevice {
    pub unsafe fn from_base(base: usize) -> Option<Self> {
        Some(Self {
            base: NonNull::new(base as *mut u8)?,
        })
    }

    pub const fn qemu_virt_base(index: usize) -> usize {
        VIRTIO_MMIO_QEMU_VIRT_BASE + (index * VIRTIO_MMIO_QEMU_VIRT_STRIDE)
    }

    pub fn base_address(&self) -> usize {
        self.base.as_ptr() as usize
    }

    pub fn magic_value(&self) -> u32 {
        self.read_reg(MMIO_MAGIC_VALUE)
    }

    pub fn version(&self) -> u32 {
        self.read_reg(MMIO_VERSION)
    }

    pub fn device_id(&self) -> u32 {
        self.read_reg(MMIO_DEVICE_ID)
    }

    pub fn vendor_id(&self) -> u32 {
        self.read_reg(MMIO_VENDOR_ID)
    }

    pub fn is_present(&self) -> bool {
        self.magic_value() == VIRTIO_MMIO_MAGIC
            && self.version() == VIRTIO_MMIO_VERSION_1
            && self.device_id() != 0
    }

    pub fn is_block_device(&self) -> bool {
        self.is_present() && self.device_id() == VIRTIO_DEVICE_ID_BLOCK
    }

    pub fn device_features(&self, selector: u32) -> u32 {
        self.write_reg(MMIO_DEVICE_FEATURES_SEL, selector);
        self.read_reg(MMIO_DEVICE_FEATURES)
    }

    pub fn set_driver_features(&mut self, selector: u32, features: u32) {
        self.write_reg(MMIO_DRIVER_FEATURES_SEL, selector);
        self.write_reg(MMIO_DRIVER_FEATURES, features);
    }

    pub fn select_queue(&mut self, queue_index: u32) {
        self.write_reg(MMIO_QUEUE_SEL, queue_index)
    }

    pub fn queue_num_max(&self) -> u32 {
        self.read_reg(MMIO_QUEUE_NUM_MAX)
    }

    pub fn set_queue_num(&mut self, queue_size: u32) {
        self.write_reg(MMIO_QUEUE_NUM, queue_size)
    }

    pub fn queue_ready(&self) -> u32 {
        self.read_reg(MMIO_QUEUE_READY)
    }

    pub fn set_queue_ready(&mut self, ready: bool) {
        self.write_reg(MMIO_QUEUE_READY, ready as u32)
    }

    pub fn set_queue_addresses(&mut self, desc: u64, driver: u64, device: u64) {
        self.write_reg64(MMIO_QUEUE_DESC_LOW, desc);
        self.write_reg64(MMIO_QUEUE_DRIVER_LOW, driver);
        self.write_reg64(MMIO_QUEUE_DEVICE_LOW, device);
    }

    pub fn notify_queue(&mut self, queue_index: u32) {
        self.write_reg(MMIO_QUEUE_NOTIFY, queue_index)
    }

    pub fn interrupt_status(&self) -> u32 {
        self.read_reg(MMIO_INTERRUPT_STATUS)
    }

    pub fn acknowledge_interrupt(&mut self, bits: u32) {
        self.write_reg(MMIO_INTERRUPT_ACK, bits)
    }

    pub fn status(&self) -> u32 {
        self.read_reg(MMIO_STATUS)
    }

    pub fn reset(&mut self) {
        self.write_reg(MMIO_STATUS, 0)
    }

    pub fn add_status(&mut self, status: u32) {
        self.write_reg(MMIO_STATUS, self.status() | status)
    }

    pub fn config_generation(&self) -> u32 {
        self.read_reg(MMIO_CONFIG_GENERATION)
    }

    fn read_reg(&self, offset: usize) -> u32 {
        unsafe { ptr::read_volatile(self.register_ptr(offset)) }
    }

    fn write_reg(&self, offset: usize, value: u32) {
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

pub fn qemu_virt_block_devices() -> QemuVirtBlockDevices {
    QemuVirtBlockDevices::new()
}

pub unsafe fn probe_qemu_virt_block_device() -> Option<VirtioMmioDevice> {
    qemu_virt_block_devices().next().map(|probe| probe.device)
}

impl VirtioBlockDriver {
    pub unsafe fn new(mut device: VirtioMmioDevice) -> Result<Self, VirtioError> {
        if !device.is_block_device() {
            return Err(VirtioError::NotBlockDevice);
        }

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

        unsafe {
            ptr::write_bytes(queue_base, 0, 4096);
        }

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
            queue,
            avail_idx: 0,
            used_idx: 0,
        })
    }
}

impl BlockDevice for VirtioBlockDriver {
    fn read_sector(&mut self, sector: u64, buffer: &mut [u8; VIRTIO_SECTOR_SIZE]) -> Result<(), VirtioError> {
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
                len: VIRTIO_SECTOR_SIZE as u32,
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

        while self.queue.used_idx() == self.used_idx {
            core::hint::spin_loop();
        }

        self.used_idx = self.used_idx.wrapping_add(1);
        fence(Ordering::SeqCst);

        let status = unsafe { ptr::read_volatile(ptr::addr_of!(VIRTIO_BLOCK_REQUEST_STATUS)) };
        if status == VIRTIO_BLK_STATUS_OK {
            Ok(())
        } else {
            Err(VirtioError::BlockRequestFailed(status))
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct VirtqDescriptor {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct VirtqUsedElem {
    pub id: u32,
    pub len: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtQueueLayout {
    pub queue_size: u16,
    pub event_idx: bool,
    pub desc_offset: usize,
    pub avail_offset: usize,
    pub used_offset: usize,
    pub total_size: usize,
}

impl VirtQueueLayout {
    pub const fn new(queue_size: u16, event_idx: bool) -> Self {
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

pub struct VirtQueue {
    base: NonNull<u8>,
    layout: VirtQueueLayout,
}

impl VirtQueue {
    pub const fn layout(queue_size: u16, event_idx: bool) -> VirtQueueLayout {
        VirtQueueLayout::new(queue_size, event_idx)
    }

    pub unsafe fn from_ptr(base: *mut u8, queue_size: u16, event_idx: bool) -> Option<Self> {
        Some(Self {
            base: NonNull::new(base)?,
            layout: VirtQueueLayout::new(queue_size, event_idx),
        })
    }

    pub const fn queue_size(&self) -> u16 {
        self.layout.queue_size
    }

    pub const fn event_idx(&self) -> bool {
        self.layout.event_idx
    }

    pub const fn total_size(&self) -> usize {
        self.layout.total_size
    }

    pub fn read_descriptor(&self, index: u16) -> VirtqDescriptor {
        debug_assert!(index < self.queue_size());
        unsafe { ptr::read_volatile(self.descriptor_ptr(index)) }
    }

    pub fn write_descriptor(&mut self, index: u16, descriptor: VirtqDescriptor) {
        debug_assert!(index < self.queue_size());
        unsafe { ptr::write_volatile(self.descriptor_mut_ptr(index), descriptor) }
    }

    pub fn avail_flags(&self) -> u16 {
        unsafe { ptr::read_volatile(self.avail_flags_ptr()) }
    }

    pub fn set_avail_flags(&mut self, flags: u16) {
        unsafe { ptr::write_volatile(self.avail_flags_mut_ptr(), flags) }
    }

    pub fn avail_idx(&self) -> u16 {
        unsafe { ptr::read_volatile(self.avail_idx_ptr()) }
    }

    pub fn set_avail_idx(&mut self, index: u16) {
        unsafe { ptr::write_volatile(self.avail_idx_mut_ptr(), index) }
    }

    pub fn avail_ring(&self, slot: u16) -> u16 {
        debug_assert!(slot < self.queue_size());
        unsafe { ptr::read_volatile(self.avail_ring_ptr(slot)) }
    }

    pub fn set_avail_ring(&mut self, slot: u16, descriptor_index: u16) {
        debug_assert!(slot < self.queue_size());
        unsafe { ptr::write_volatile(self.avail_ring_mut_ptr(slot), descriptor_index) }
    }

    pub fn used_flags(&self) -> u16 {
        unsafe { ptr::read_volatile(self.used_flags_ptr()) }
    }

    pub fn set_used_flags(&mut self, flags: u16) {
        unsafe { ptr::write_volatile(self.used_flags_mut_ptr(), flags) }
    }

    pub fn used_idx(&self) -> u16 {
        unsafe { ptr::read_volatile(self.used_idx_ptr()) }
    }

    pub fn set_used_idx(&mut self, index: u16) {
        unsafe { ptr::write_volatile(self.used_idx_mut_ptr(), index) }
    }

    pub fn used_elem(&self, slot: u16) -> VirtqUsedElem {
        debug_assert!(slot < self.queue_size());
        unsafe { ptr::read_volatile(self.used_ring_ptr(slot)) }
    }

    pub fn set_used_elem(&mut self, slot: u16, elem: VirtqUsedElem) {
        debug_assert!(slot < self.queue_size());
        unsafe { ptr::write_volatile(self.used_ring_mut_ptr(slot), elem) }
    }

    pub fn used_event(&self) -> Option<u16> {
        if !self.event_idx() {
            return None;
        }

        Some(unsafe { ptr::read_volatile(self.used_event_ptr()) })
    }

    pub fn set_used_event(&mut self, event: u16) {
        debug_assert!(self.event_idx());
        unsafe { ptr::write_volatile(self.used_event_mut_ptr(), event) }
    }

    pub fn avail_event(&self) -> Option<u16> {
        if !self.event_idx() {
            return None;
        }

        Some(unsafe { ptr::read_volatile(self.avail_event_ptr()) })
    }

    pub fn set_avail_event(&mut self, event: u16) {
        debug_assert!(self.event_idx());
        unsafe { ptr::write_volatile(self.avail_event_mut_ptr(), event) }
    }

    fn descriptor_ptr(&self, index: u16) -> *const VirtqDescriptor {
        unsafe { self.base.as_ptr().add(self.layout.desc_offset + index as usize * mem::size_of::<VirtqDescriptor>()) as *const VirtqDescriptor }
    }

    fn descriptor_mut_ptr(&mut self, index: u16) -> *mut VirtqDescriptor {
        self.descriptor_ptr(index) as *mut VirtqDescriptor
    }

    fn avail_flags_ptr(&self) -> *const u16 {
        unsafe { self.base.as_ptr().add(self.layout.avail_offset) as *const u16 }
    }

    fn avail_flags_mut_ptr(&mut self) -> *mut u16 {
        self.avail_flags_ptr() as *mut u16
    }

    fn avail_idx_ptr(&self) -> *const u16 {
        unsafe { self.base.as_ptr().add(self.layout.avail_offset + 2) as *const u16 }
    }

    fn avail_idx_mut_ptr(&mut self) -> *mut u16 {
        self.avail_idx_ptr() as *mut u16
    }

    fn avail_ring_ptr(&self, slot: u16) -> *const u16 {
        unsafe { self.base.as_ptr().add(self.layout.avail_offset + 4 + slot as usize * 2) as *const u16 }
    }

    fn avail_ring_mut_ptr(&mut self, slot: u16) -> *mut u16 {
        self.avail_ring_ptr(slot) as *mut u16
    }

    fn used_flags_ptr(&self) -> *const u16 {
        unsafe { self.base.as_ptr().add(self.layout.used_offset) as *const u16 }
    }

    fn used_flags_mut_ptr(&mut self) -> *mut u16 {
        self.used_flags_ptr() as *mut u16
    }

    fn used_idx_ptr(&self) -> *const u16 {
        unsafe { self.base.as_ptr().add(self.layout.used_offset + 2) as *const u16 }
    }

    fn used_idx_mut_ptr(&mut self) -> *mut u16 {
        self.used_idx_ptr() as *mut u16
    }

    fn used_ring_ptr(&self, slot: u16) -> *const VirtqUsedElem {
        unsafe {
            self.base.as_ptr().add(self.layout.used_offset + 4 + slot as usize * mem::size_of::<VirtqUsedElem>())
                as *const VirtqUsedElem
        }
    }

    fn used_ring_mut_ptr(&mut self, slot: u16) -> *mut VirtqUsedElem {
        self.used_ring_ptr(slot) as *mut VirtqUsedElem
    }

    fn used_event_ptr(&self) -> *const u16 {
        unsafe { self.base.as_ptr().add(self.layout.avail_offset + 4 + self.queue_size() as usize * 2) as *const u16 }
    }

    fn used_event_mut_ptr(&mut self) -> *mut u16 {
        self.used_event_ptr() as *mut u16
    }

    fn avail_event_ptr(&self) -> *const u16 {
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