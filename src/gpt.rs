//! GPT parsing for block devices.
//!
//! This module reads the primary GPT header and fixed-size partition entries from
//! a sector-addressable block device. It is intentionally small and currently
//! only supports the data needed by the firmware diagnostics output.

use core::cmp::min;
use core::str;

use crate::partition::{PartitionEntry, PartitionTable};
use crate::virtio::{BlockDevice, VIRTIO_SECTOR_SIZE};

/// GPT header signature stored at the start of the primary header sector.
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
/// Minimum supported GPT header size in bytes.
const GPT_HEADER_MIN_SIZE: usize = 92;
/// Offset of the GPT header CRC32 field.
const GPT_HEADER_CRC_OFFSET: usize = 16;
/// Offset of the current-header-LBA field.
const GPT_HEADER_LBA_OFFSET: usize = 24;
/// Offset of the backup-header-LBA field.
const GPT_BACKUP_LBA_OFFSET: usize = 32;
/// Offset of the first usable LBA field.
const GPT_FIRST_USABLE_LBA_OFFSET: usize = 40;
/// Offset of the last usable LBA field.
const GPT_LAST_USABLE_LBA_OFFSET: usize = 48;
/// Offset of the partition entry array starting LBA.
const GPT_PARTITION_ENTRY_LBA_OFFSET: usize = 72;
/// Offset of the partition entry count.
const GPT_PARTITION_ENTRY_COUNT_OFFSET: usize = 80;
/// Offset of the partition entry size.
const GPT_PARTITION_ENTRY_SIZE_OFFSET: usize = 84;
/// Offset of the partition entry array CRC32.
const GPT_PARTITION_ENTRY_ARRAY_CRC_OFFSET: usize = 88;
/// Number of UTF-16 code units stored in the GPT partition name field.
const GPT_PARTITION_NAME_LEN: usize = 36;
/// Minimum supported GPT partition entry size in bytes.
const GPT_ENTRY_MIN_SIZE: usize = 128;
/// Maximum temporary read size needed to decode one GPT entry.
const GPT_ENTRY_READ_BUFFER_SIZE: usize = VIRTIO_SECTOR_SIZE * 2;
/// GPT attribute bit indicating legacy BIOS bootability.
const GPT_ATTRIBUTE_LEGACY_BIOS_BOOTABLE: u64 = 1 << 2;
/// Partition type GUID for a generic Linux filesystem partition.
const GPT_TYPE_GUID_LINUX_FILESYSTEM: [u8; 16] = [
    0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4,
];
/// Partition type GUID for the EFI system partition.
const GPT_TYPE_GUID_ESP: [u8; 16] = [
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
];

/// Parsed fields from the primary GPT header used by this firmware.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GptHeader {
    /// Starting LBA of the partition entry array.
    pub partition_entry_lba: u64,
    /// Number of partition entries described by the header.
    pub partition_entry_count: u32,
    /// Size in bytes of each partition entry.
    pub partition_entry_size: u32,
}

/// Parsed GPT partition entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GptPartitionEntry {
    /// Partition type GUID stored in little-endian GPT byte order.
    pub partition_type_guid: [u8; 16],
    /// Unique GUID assigned to this partition entry.
    pub unique_partition_guid: [u8; 16],
    /// First LBA owned by the partition.
    pub first_lba: u64,
    /// Last LBA owned by the partition.
    pub last_lba: u64,
    /// GPT attribute bitfield for the partition.
    pub attributes: u64,
    /// UTF-16LE partition name as stored in the GPT entry.
    partition_name: [u16; GPT_PARTITION_NAME_LEN],
}

/// GPT-backed partition table implementation.
pub struct GptPartitionTable<'a, D: BlockDevice> {
    /// Device containing the GPT metadata and partition-entry array.
    device: &'a mut D,
    /// Parsed primary header used to locate partition entries.
    header: GptHeader,
}

impl<'a, D: BlockDevice> GptPartitionTable<'a, D> {
    /// Creates a GPT partition table view from the primary header.
    ///
    /// # Parameters
    ///
    /// - `device`: Sector-addressable block device containing the GPT.
    pub fn new(device: &'a mut D) -> Option<Self> {
        let header = read_primary_header(device)
            .or_else(|| read_backup_header(device))?;
        Some(Self { device, header })
    }
}

/// Reads the primary GPT header from LBA 1.
///
/// # Parameters
///
/// - `device`: Sector-addressable block device containing the GPT.
pub fn read_primary_header<D: BlockDevice>(device: &mut D) -> Option<GptHeader> {
    read_header_at(device, 1)
}

/// Reads the backup GPT header from the last sector of the device.
fn read_backup_header<D: BlockDevice>(device: &mut D) -> Option<GptHeader> {
    let last_sector = device.sector_count().checked_sub(1)?;
    read_header_at(device, last_sector)
}

/// Reads and validates one GPT header at an explicit LBA.
fn read_header_at<D: BlockDevice>(device: &mut D, header_lba: u64) -> Option<GptHeader> {
    let mut sector = [0u8; VIRTIO_SECTOR_SIZE];
    device.read_blocks(header_lba, &mut sector).ok()?;

    if &sector[0..8] != GPT_SIGNATURE {
        return None;
    }

    let header_size = read_u32(&sector, 12)? as usize;
    if !(GPT_HEADER_MIN_SIZE..=VIRTIO_SECTOR_SIZE).contains(&header_size) {
        return None;
    }

    let header_crc = read_u32(&sector, GPT_HEADER_CRC_OFFSET)?;
    let mut header_bytes = [0u8; VIRTIO_SECTOR_SIZE];
    header_bytes[..header_size].copy_from_slice(&sector[..header_size]);
    header_bytes[GPT_HEADER_CRC_OFFSET..GPT_HEADER_CRC_OFFSET + 4]
        .copy_from_slice(&[0u8; 4]);
    if crc32(&header_bytes[..header_size]) != header_crc {
        return None;
    }

    let current_lba = read_u64(&sector, GPT_HEADER_LBA_OFFSET)?;
    if current_lba != header_lba {
        return None;
    }

    let backup_lba = read_u64(&sector, GPT_BACKUP_LBA_OFFSET)?;
    if backup_lba >= device.sector_count() || backup_lba == current_lba {
        return None;
    }

    let first_usable_lba = read_u64(&sector, GPT_FIRST_USABLE_LBA_OFFSET)?;
    let last_usable_lba = read_u64(&sector, GPT_LAST_USABLE_LBA_OFFSET)?;
    if first_usable_lba > last_usable_lba || last_usable_lba >= device.sector_count() {
        return None;
    }

    let partition_entry_lba = read_u64(&sector, GPT_PARTITION_ENTRY_LBA_OFFSET)?;
    let partition_entry_count = read_u32(&sector, GPT_PARTITION_ENTRY_COUNT_OFFSET)?;
    let entry_size = read_u32(&sector, GPT_PARTITION_ENTRY_SIZE_OFFSET)?;
    if partition_entry_count == 0
        || entry_size < GPT_ENTRY_MIN_SIZE as u32
        || (entry_size % GPT_ENTRY_MIN_SIZE as u32) != 0
    {
        return None;
    }

    let entry_array_bytes = u64::from(partition_entry_count).checked_mul(u64::from(entry_size))?;
    let entry_array_sectors = entry_array_bytes.div_ceil(VIRTIO_SECTOR_SIZE as u64);
    let entry_array_end_lba = partition_entry_lba.checked_add(entry_array_sectors)?;
    if partition_entry_lba == 0 || entry_array_end_lba > device.sector_count() {
        return None;
    }

    if header_lba >= partition_entry_lba && header_lba < entry_array_end_lba {
        return None;
    }

    let entry_array_crc = read_u32(&sector, GPT_PARTITION_ENTRY_ARRAY_CRC_OFFSET)?;
    if entry_array_crc != validate_entry_array_crc(device, partition_entry_lba, entry_array_bytes, entry_array_crc)? {
        return None;
    }

    Some(GptHeader {
        partition_entry_lba,
        partition_entry_count,
        partition_entry_size: entry_size,
    })
}

/// Computes the CRC32 of the GPT entry array and returns it.
fn validate_entry_array_crc<D: BlockDevice>(
    device: &mut D,
    start_lba: u64,
    byte_len: u64,
    expected_crc: u32,
) -> Option<u32> {
    let mut sector = [0u8; VIRTIO_SECTOR_SIZE];
    let mut current_lba = start_lba;
    let mut remaining = usize::try_from(byte_len).ok()?;
    let mut crc = 0xffff_ffff;

    while remaining != 0 {
        device.read_blocks(current_lba, &mut sector).ok()?;
        let take = min(remaining, VIRTIO_SECTOR_SIZE);
        crc = crc32_update(crc, &sector[..take]);
        remaining -= take;
        current_lba = current_lba.checked_add(1)?;
    }

    let computed = crc32_finalize(crc);
    if computed == expected_crc {
        Some(computed)
    } else {
        None
    }
}

/// Reads one GPT partition entry by index.
///
/// # Parameters
///
/// - `device`: Sector-addressable block device containing the GPT.
/// - `header`: Parsed GPT header describing the partition entry array.
/// - `index`: Zero-based partition entry index to decode.
pub fn read_partition_entry<D: BlockDevice>(
    device: &mut D,
    header: &GptHeader,
    index: u32,
) -> Option<GptPartitionEntry> {
    if index >= header.partition_entry_count {
        return None;
    }

    let mut sectors = [0u8; GPT_ENTRY_READ_BUFFER_SIZE];
    let entry_size = header.partition_entry_size as usize;
    let entry_offset = index as usize * entry_size;
    let sector_lba = header.partition_entry_lba + (entry_offset / VIRTIO_SECTOR_SIZE) as u64;
    let sector_offset = entry_offset % VIRTIO_SECTOR_SIZE;

    let sectors_to_read = required_entry_sectors(sector_offset)?;
    let bytes_to_read = sectors_to_read * VIRTIO_SECTOR_SIZE;

    device.read_blocks(sector_lba, &mut sectors[..bytes_to_read]).ok()?;
    let entry = &sectors[sector_offset..sector_offset + GPT_ENTRY_MIN_SIZE];

    let mut partition_name = [0u16; GPT_PARTITION_NAME_LEN];
    let mut name_index = 0;
    while name_index < GPT_PARTITION_NAME_LEN {
        let offset = 56 + (name_index * 2);
        partition_name[name_index] = u16::from_le_bytes([entry[offset], entry[offset + 1]]);
        name_index += 1;
    }

    Some(GptPartitionEntry {
        partition_type_guid: copy_16(entry, 0)?,
        unique_partition_guid: copy_16(entry, 16)?,
        first_lba: read_u64(entry, 32)?,
        last_lba: read_u64(entry, 40)?,
        attributes: read_u64(entry, 48)?,
        partition_name,
    })
}

/// Returns the number of sectors that must be read to decode one GPT entry.
///
/// # Parameters
///
/// - `sector_offset`: Byte offset of the entry within its starting sector.
fn required_entry_sectors(sector_offset: usize) -> Option<usize> {
    let bytes_needed = sector_offset.checked_add(GPT_ENTRY_MIN_SIZE)?;
    Some(bytes_needed.div_ceil(VIRTIO_SECTOR_SIZE))
}

impl GptPartitionEntry {
    /// Returns `true` when the partition slot is unused.
    pub fn is_unused(&self) -> bool {
        self.partition_type_guid == [0; 16]
    }

    /// Returns the number of sectors covered by the partition.
    pub fn sector_count(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }

    /// Returns `true` when the legacy BIOS bootable attribute is set.
    pub fn bootable(&self) -> bool {
        (self.attributes & GPT_ATTRIBUTE_LEGACY_BIOS_BOOTABLE) != 0
    }

    /// Decodes the UTF-16 partition label into a printable ASCII string.
    ///
    /// # Parameters
    ///
    /// - `buffer`: Scratch buffer that receives the printable label bytes.
    pub fn label<'a>(&self, buffer: &'a mut [u8; 72]) -> &'a str {
        let mut out = 0;

        for code_unit in self.partition_name {
            if code_unit == 0 || out == buffer.len() {
                break;
            }

            buffer[out] = if code_unit <= 0x7f { code_unit as u8 } else { b'?' };
            out += 1;
        }

        if out == 0 {
            buffer[0] = b'-';
            out = 1;
        }

        unsafe { str::from_utf8_unchecked(&buffer[..out]) }
    }

    /// Returns a friendly partition type name for known GUIDs and the raw GUID
    /// string for unknown ones.
    ///
    /// # Parameters
    ///
    /// - `buffer`: Scratch buffer used when formatting an unknown GUID.
    pub fn partition_type<'a>(&self, buffer: &'a mut [u8; 36]) -> &'a str {
        if self.partition_type_guid == GPT_TYPE_GUID_LINUX_FILESYSTEM {
            return "Linux filesystem";
        }

        if self.partition_type_guid == GPT_TYPE_GUID_ESP {
            return "ESP";
        }

        self.guid(buffer)
    }

    /// Formats the raw partition-type GUID as a canonical string.
    ///
    /// # Parameters
    ///
    /// - `buffer`: Scratch buffer that receives the formatted GUID text.
    fn guid<'a>(&self, buffer: &'a mut [u8; 36]) -> &'a str {
        /// Hex digit lookup table used when formatting GUID bytes.
        const HEX: &[u8; 16] = b"0123456789abcdef";

        let guid = self.partition_type_guid;
        let bytes = [
            guid[3], guid[2], guid[1], guid[0], 0xff,
            guid[5], guid[4], 0xff,
            guid[7], guid[6], 0xff,
            guid[8], guid[9], 0xff,
            guid[10], guid[11], guid[12], guid[13], guid[14], guid[15],
        ];

        let mut src = 0;
        let mut out = 0;
        while src < bytes.len() {
            if bytes[src] == 0xff {
                buffer[out] = b'-';
                out += 1;
            } else {
                buffer[out] = HEX[(bytes[src] >> 4) as usize];
                buffer[out + 1] = HEX[(bytes[src] & 0x0f) as usize];
                out += 2;
            }

            src += 1;
        }

        unsafe { str::from_utf8_unchecked(&buffer[..out]) }
    }
}

impl PartitionEntry for GptPartitionEntry {
    /// Returns `true` when the GPT slot is populated.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn is_present(&self) -> bool {
        !self.is_unused()
    }

    /// Returns the first logical block address of the partition.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn first_lba(&self) -> u64 {
        self.first_lba
    }

    /// Returns the last logical block address of the partition.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn last_lba(&self) -> u64 {
        self.last_lba
    }

    /// Returns the number of sectors covered by the partition.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn sector_count(&self) -> u64 {
        GptPartitionEntry::sector_count(self)
    }

    /// Returns `true` when the legacy BIOS bootable attribute is set.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn bootable(&self) -> bool {
        GptPartitionEntry::bootable(self)
    }

    /// Returns `true` when the partition type GUID is the ESP GUID.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn is_efi_system_partition(&self) -> bool {
        self.partition_type_guid == GPT_TYPE_GUID_ESP
    }

    /// Formats the partition label into the provided scratch buffer.
    ///
    /// # Parameters
    ///
    /// - `buffer`: Scratch buffer that receives the partition label.
    fn label<'a>(&self, buffer: &'a mut [u8; 72]) -> &'a str {
        GptPartitionEntry::label(self, buffer)
    }

    /// Formats the partition type into the provided scratch buffer.
    ///
    /// # Parameters
    ///
    /// - `buffer`: Scratch buffer that receives the partition type.
    fn partition_type<'a>(&self, buffer: &'a mut [u8; 36]) -> &'a str {
        GptPartitionEntry::partition_type(self, buffer)
    }
}

impl<D: BlockDevice> PartitionTable for GptPartitionTable<'_, D> {
    type Entry = GptPartitionEntry;

    /// Returns the number of GPT partition-entry slots.
    ///
    /// # Parameters
    ///
    /// This method does not accept parameters.
    fn partition_count(&self) -> u32 {
        self.header.partition_entry_count
    }

    /// Reads one GPT partition entry by zero-based index.
    ///
    /// # Parameters
    ///
    /// - `index`: Zero-based partition entry index to decode.
    fn partition(&mut self, index: u32) -> Option<Self::Entry> {
        read_partition_entry(self.device, &self.header, index)
    }
}

/// Reads one little-endian `u32` from `bytes`.
///
/// # Parameters
///
/// - `bytes`: Byte slice containing the encoded value.
/// - `offset`: Starting byte offset of the value.
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let data = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

/// Reads one little-endian `u64` from `bytes`.
///
/// # Parameters
///
/// - `bytes`: Byte slice containing the encoded value.
/// - `offset`: Starting byte offset of the value.
fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let data = bytes.get(offset..offset + 8)?;
    Some(u64::from_le_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]))
}

/// Copies 16 bytes from `bytes` starting at `offset`.
///
/// # Parameters
///
/// - `bytes`: Byte slice containing the source data.
/// - `offset`: Starting byte offset of the 16-byte field.
fn copy_16(bytes: &[u8], offset: usize) -> Option<[u8; 16]> {
    let data = bytes.get(offset..offset + 16)?;
    let mut value = [0u8; 16];
    value.copy_from_slice(data);
    Some(value)
}

/// Computes one GPT-style IEEE CRC32 over `bytes`.
fn crc32(bytes: &[u8]) -> u32 {
    crc32_finalize(crc32_update(0xffff_ffff, bytes))
}

/// Updates an in-progress IEEE CRC32 with `bytes`.
fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    let mut index = 0usize;
    while index < bytes.len() {
        crc ^= u32::from(bytes[index]);

        let mut bit = 0usize;
        while bit < 8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xedb8_8320;
            } else {
                crc >>= 1;
            }
            bit += 1;
        }

        index += 1;
    }

    crc
}

/// Finalizes one in-progress CRC32 state.
fn crc32_finalize(crc: u32) -> u32 {
    !crc
}