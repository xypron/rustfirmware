//! GPT parsing for block devices.
//!
//! This module reads the primary GPT header and fixed-size partition entries from
//! a sector-addressable block device. It is intentionally small and currently
//! only supports the data needed by the firmware diagnostics output.

use core::str;

use crate::partition::{PartitionEntry, PartitionTable};
use crate::virtio::{BlockDevice, VIRTIO_SECTOR_SIZE};

/// GPT header signature stored at the start of the primary header sector.
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
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
        let header = read_primary_header(device)?;
        Some(Self { device, header })
    }
}

/// Reads the primary GPT header from LBA 1.
///
/// # Parameters
///
/// - `device`: Sector-addressable block device containing the GPT.
pub fn read_primary_header<D: BlockDevice>(device: &mut D) -> Option<GptHeader> {
    let mut sector = [0u8; VIRTIO_SECTOR_SIZE];
    device.read_blocks(1, &mut sector).ok()?;

    if &sector[0..8] != GPT_SIGNATURE {
        return None;
    }

    let entry_size = read_u32(&sector, 84)?;
    if entry_size < GPT_ENTRY_MIN_SIZE as u32 {
        return None;
    }

    Some(GptHeader {
        partition_entry_lba: read_u64(&sector, 72)?,
        partition_entry_count: read_u32(&sector, 80)?,
        partition_entry_size: entry_size,
    })
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