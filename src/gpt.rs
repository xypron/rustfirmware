use core::str;

use crate::virtio::{BlockDevice, VIRTIO_SECTOR_SIZE};

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_PARTITION_NAME_LEN: usize = 36;
const GPT_ENTRY_MIN_SIZE: usize = 128;
const GPT_ATTRIBUTE_LEGACY_BIOS_BOOTABLE: u64 = 1 << 2;
const GPT_TYPE_GUID_LINUX_FILESYSTEM: [u8; 16] = [
    0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4,
];
const GPT_TYPE_GUID_ESP: [u8; 16] = [
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GptHeader {
    pub partition_entry_lba: u64,
    pub partition_entry_count: u32,
    pub partition_entry_size: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GptPartitionEntry {
    pub partition_type_guid: [u8; 16],
    pub unique_partition_guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
    partition_name: [u16; GPT_PARTITION_NAME_LEN],
}

pub fn read_primary_header<D: BlockDevice>(device: &mut D) -> Option<GptHeader> {
    let mut sector = [0u8; VIRTIO_SECTOR_SIZE];
    device.read_sector(1, &mut sector).ok()?;

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

pub fn read_partition_entry<D: BlockDevice>(
    device: &mut D,
    header: &GptHeader,
    index: u32,
) -> Option<GptPartitionEntry> {
    if index >= header.partition_entry_count {
        return None;
    }

    let mut sector = [0u8; VIRTIO_SECTOR_SIZE];
    let entry_size = header.partition_entry_size as usize;
    let entry_offset = index as usize * entry_size;
    let sector_lba = header.partition_entry_lba + (entry_offset / VIRTIO_SECTOR_SIZE) as u64;
    let sector_offset = entry_offset % VIRTIO_SECTOR_SIZE;

    if sector_offset + GPT_ENTRY_MIN_SIZE > VIRTIO_SECTOR_SIZE {
        return None;
    }

    device.read_sector(sector_lba, &mut sector).ok()?;
    let entry = &sector[sector_offset..sector_offset + GPT_ENTRY_MIN_SIZE];

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

impl GptPartitionEntry {
    pub fn is_unused(&self) -> bool {
        self.partition_type_guid == [0; 16]
    }

    pub fn sector_count(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }

    pub fn bootable(&self) -> bool {
        (self.attributes & GPT_ATTRIBUTE_LEGACY_BIOS_BOOTABLE) != 0
    }

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

    pub fn partition_type<'a>(&self, buffer: &'a mut [u8; 36]) -> &'a str {
        if self.partition_type_guid == GPT_TYPE_GUID_LINUX_FILESYSTEM {
            return "Linux filesystem";
        }

        if self.partition_type_guid == GPT_TYPE_GUID_ESP {
            return "ESP";
        }

        self.guid(buffer)
    }

    fn guid<'a>(&self, buffer: &'a mut [u8; 36]) -> &'a str {
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

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let data = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let data = bytes.get(offset..offset + 8)?;
    Some(u64::from_le_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]))
}

fn copy_16(bytes: &[u8], offset: usize) -> Option<[u8; 16]> {
    let data = bytes.get(offset..offset + 16)?;
    let mut value = [0u8; 16];
    value.copy_from_slice(data);
    Some(value)
}