//! Read-only FAT filesystem support.
//!
//! This module mounts FAT16 and FAT32 volumes from a partition-relative start
//! sector, traverses nested directory paths, understands VFAT long file names,
//! and reads complete file contents into a caller-supplied buffer.

use core::char;
use core::cmp::min;
use core::str;

use crate::virtio::{BlockDevice, VirtioError, VIRTIO_SECTOR_SIZE};

/// Number of UTF-16 code units reserved for one long FAT file name.
const FAT_LONG_NAME_CODE_UNITS: usize = 260;
/// Maximum UTF-8 bytes needed to encode one long FAT file name.
const FAT_LONG_NAME_UTF8_BYTES: usize = FAT_LONG_NAME_CODE_UNITS * 3;
/// Maximum UTF-8 bytes used to build one recursive file path.
const FAT_PATH_UTF8_BYTES: usize = 1024;
/// Size in bytes of one FAT directory entry.
const FAT_DIR_ENTRY_SIZE: usize = 32;
/// Attribute bit indicating that an entry is a volume label.
const FAT_ATTRIBUTE_VOLUME_ID: u8 = 0x08;
/// Attribute bit indicating that an entry is a subdirectory.
const FAT_ATTRIBUTE_DIRECTORY: u8 = 0x10;
/// Attribute value used by VFAT long-name entries.
const FAT_ATTRIBUTE_LONG_NAME: u8 = 0x0f;
/// End-of-chain marker threshold for FAT16 cluster chains.
const FAT16_END_OF_CHAIN: u32 = 0xfff8;
/// Bad-cluster marker value for FAT16 cluster chains.
const FAT16_BAD_CLUSTER: u32 = 0xfff7;
/// End-of-chain marker threshold for FAT32 cluster chains.
const FAT32_END_OF_CHAIN: u32 = 0x0fff_fff8;
/// Bad-cluster marker value for FAT32 cluster chains.
const FAT32_BAD_CLUSTER: u32 = 0x0fff_fff7;

/// Read-only FAT variants supported by this module.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FatType {
    /// FAT16 with a fixed root directory region.
    Fat16,
    /// FAT32 with the root directory stored in a cluster chain.
    Fat32,
}

/// Errors returned by the FAT filesystem reader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FatError {
    /// The underlying block device reported an I/O failure.
    Device(VirtioError),
    /// The boot sector did not describe a supported FAT filesystem.
    InvalidBootSector,
    /// The mounted volume uses a sector size other than 512 bytes.
    UnsupportedSectorSize(u16),
    /// The mounted volume is FAT12, which this module does not support.
    UnsupportedFatType,
    /// The requested path was empty or otherwise malformed.
    InvalidPath,
    /// A required path component or file was not found.
    NotFound,
    /// A path component that should have been a directory was not one.
    NotDirectory,
    /// The resolved path points at a directory instead of a file.
    IsDirectory,
    /// The caller-supplied output buffer is too small for the file.
    BufferTooSmall,
    /// The filesystem contained an invalid or out-of-range cluster number.
    InvalidCluster(u32),
    /// A VFAT long-name sequence was malformed.
    InvalidLongName,
    /// A decoded file name exceeded the supported in-memory limit.
    NameTooLong,
}

impl From<VirtioError> for FatError {
    /// Converts one block-device error into the matching FAT-layer error.
    ///
    /// # Parameters
    ///
    /// - `error`: Block-device error to wrap.
    fn from(error: VirtioError) -> Self {
        Self::Device(error)
    }
}

/// Mounted read-only FAT volume backed by a block device.
pub struct FatVolume<'a, D: BlockDevice> {
    /// Underlying sector-addressable block device.
    device: &'a mut D,
    /// Partition-relative start sector of the mounted volume.
    partition_start_lba: u64,
    /// Supported FAT variant derived from the BPB.
    fat_type: FatType,
    /// Number of sectors stored in one allocation cluster.
    sectors_per_cluster: u8,
    /// Number of reserved sectors before the FAT region.
    reserved_sector_count: u16,
    /// Size in sectors of the fixed FAT16 root-directory region.
    root_dir_sectors: u32,
    /// First sector of the FAT16 root-directory region.
    root_dir_first_sector: u32,
    /// First data sector containing cluster number `2`.
    data_first_sector: u32,
    /// Root-directory start cluster on FAT32 volumes.
    root_cluster: u32,
    /// Total number of addressable data clusters on the volume.
    total_clusters: u32,
}

/// Directory entry resolved while traversing a FAT path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FatDirectoryEntry {
    /// Attribute byte copied from the directory entry.
    attributes: u8,
    /// First cluster of the file or directory data.
    first_cluster: u32,
    /// Logical file size in bytes.
    file_size: u32,
}

impl FatDirectoryEntry {
    /// Returns `true` when this directory entry represents a directory.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn is_directory(&self) -> bool {
        (self.attributes & FAT_ATTRIBUTE_DIRECTORY) != 0
    }
}

/// Directory location used while traversing nested paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectoryLocation {
    /// Root directory of the mounted filesystem.
    RootDirectory,
    /// Regular directory stored in a cluster chain.
    ClusterChain(u32),
}

/// Result of scanning one directory sector for a matching entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectoryScanResult {
    /// The desired entry was found in the current sector.
    Found(FatDirectoryEntry),
    /// More directory entries may appear in later sectors.
    Continue,
    /// The end-of-directory marker was reached.
    EndOfDirectory,
}

/// State used to assemble one VFAT long-name sequence.
struct LongNameState {
    /// Decoded UTF-16 code units stored by VFAT directory entries.
    code_units: [u16; FAT_LONG_NAME_CODE_UNITS],
    /// Highest ordinal value announced by the long-name sequence.
    max_ordinal: u8,
    /// Bitmask of long-name ordinals already observed.
    ordinal_mask: u32,
    /// Short-name checksum carried by the long-name entries.
    checksum: u8,
    /// Whether a potentially valid long-name sequence is in progress.
    active: bool,
}

impl LongNameState {
    /// Creates an empty long-name assembly state.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    const fn new() -> Self {
        Self {
            code_units: [0xffff; FAT_LONG_NAME_CODE_UNITS],
            max_ordinal: 0,
            ordinal_mask: 0,
            checksum: 0,
            active: false,
        }
    }

    /// Clears any partially assembled long-name sequence.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    fn clear(&mut self) {
        self.code_units = [0xffff; FAT_LONG_NAME_CODE_UNITS];
        self.max_ordinal = 0;
        self.ordinal_mask = 0;
        self.checksum = 0;
        self.active = false;
    }

    /// Appends one VFAT long-name entry into the assembly state.
    ///
    /// # Parameters
    ///
    /// - `entry`: Raw 32-byte long-name directory entry.
    fn push(&mut self, entry: &[u8; FAT_DIR_ENTRY_SIZE]) -> Result<(), FatError> {
        let ordinal = entry[0] & 0x1f;
        let is_last = (entry[0] & 0x40) != 0;

        if ordinal == 0 {
            self.clear();
            return Err(FatError::InvalidLongName);
        }

        if is_last {
            self.clear();
            self.active = true;
            self.max_ordinal = ordinal;
            self.checksum = entry[13];
        }

        if !self.active || ordinal > self.max_ordinal || entry[13] != self.checksum {
            self.clear();
            return Err(FatError::InvalidLongName);
        }

        let start = (ordinal as usize - 1) * 13;
        if start + 13 > self.code_units.len() {
            self.clear();
            return Err(FatError::NameTooLong);
        }

        copy_lfn_code_units(entry, &mut self.code_units[start..start + 13]);
        self.ordinal_mask |= 1u32 << (ordinal - 1);
        Ok(())
    }

    /// Returns `true` when the assembled long-name sequence matches `entry`.
    ///
    /// # Parameters
    ///
    /// - `entry`: Short directory entry that terminates the long-name sequence.
    fn matches_entry(&self, entry: &[u8; FAT_DIR_ENTRY_SIZE]) -> bool {
        if !self.active || self.max_ordinal == 0 || self.max_ordinal > 20 {
            return false;
        }

        let expected_mask = (1u32 << self.max_ordinal) - 1;
        self.ordinal_mask == expected_mask
            && self.checksum == short_name_checksum(&entry[..11])
    }

    /// Decodes the assembled long name into UTF-8.
    ///
    /// # Parameters
    ///
    /// - `buffer`: Output buffer that receives the UTF-8 name bytes.
    fn to_utf8<'a>(
        &self,
        buffer: &'a mut [u8; FAT_LONG_NAME_UTF8_BYTES],
    ) -> Result<&'a str, FatError> {
        let mut input_index = 0usize;
        let mut output_index = 0usize;

        while input_index < self.code_units.len() {
            let code_unit = self.code_units[input_index];
            if code_unit == 0x0000 || code_unit == 0xffff {
                break;
            }

            let character = if (0xd800..=0xdbff).contains(&code_unit) {
                let next = *self
                    .code_units
                    .get(input_index + 1)
                    .ok_or(FatError::InvalidLongName)?;
                if !(0xdc00..=0xdfff).contains(&next) {
                    return Err(FatError::InvalidLongName);
                }

                input_index += 1;
                let code_point = 0x10000
                    + ((((code_unit as u32) - 0xd800) << 10)
                        | ((next as u32) - 0xdc00));
                char::from_u32(code_point).ok_or(FatError::InvalidLongName)?
            } else if (0xdc00..=0xdfff).contains(&code_unit) {
                return Err(FatError::InvalidLongName);
            } else {
                char::from_u32(code_unit as u32)
                    .ok_or(FatError::InvalidLongName)?
            };

            let encoded_len = character.len_utf8();
            if output_index + encoded_len > buffer.len() {
                return Err(FatError::NameTooLong);
            }

            character.encode_utf8(&mut buffer[output_index..output_index + encoded_len]);
            output_index += encoded_len;
            input_index += 1;
        }

        str::from_utf8(&buffer[..output_index]).map_err(|_| FatError::InvalidLongName)
    }
}

impl<'a, D: BlockDevice> FatVolume<'a, D> {
    /// Mounts a FAT16 or FAT32 filesystem from a partition start sector.
    ///
    /// # Parameters
    ///
    /// - `device`: Underlying block device that backs the FAT volume.
    /// - `partition_start_lba`: First sector of the filesystem partition.
    pub fn new(
        device: &'a mut D,
        partition_start_lba: u64,
    ) -> Result<Self, FatError> {
        let mut boot_sector = [0u8; VIRTIO_SECTOR_SIZE];
        device.read_blocks(partition_start_lba, &mut boot_sector)?;

        let bytes_per_sector = read_u16(&boot_sector, 11)
            .ok_or(FatError::InvalidBootSector)?;
        if bytes_per_sector as usize != VIRTIO_SECTOR_SIZE {
            return Err(FatError::UnsupportedSectorSize(bytes_per_sector));
        }

        let sectors_per_cluster = *boot_sector.get(13)
            .ok_or(FatError::InvalidBootSector)?;
        let reserved_sector_count = read_u16(&boot_sector, 14)
            .ok_or(FatError::InvalidBootSector)?;
        let fat_count = *boot_sector.get(16)
            .ok_or(FatError::InvalidBootSector)?;
        let root_dir_entries = read_u16(&boot_sector, 17)
            .ok_or(FatError::InvalidBootSector)?;
        let total_sectors_16 = read_u16(&boot_sector, 19)
            .ok_or(FatError::InvalidBootSector)? as u32;
        let fat_size_16 = read_u16(&boot_sector, 22)
            .ok_or(FatError::InvalidBootSector)? as u32;
        let total_sectors_32 = read_u32(&boot_sector, 32)
            .ok_or(FatError::InvalidBootSector)?;
        let fat_size_32 = read_u32(&boot_sector, 36)
            .ok_or(FatError::InvalidBootSector)?;
        let root_cluster = read_u32(&boot_sector, 44)
            .ok_or(FatError::InvalidBootSector)?;

        if sectors_per_cluster == 0 || reserved_sector_count == 0 || fat_count == 0 {
            return Err(FatError::InvalidBootSector);
        }

        let total_sectors = if total_sectors_16 != 0 {
            total_sectors_16
        } else {
            total_sectors_32
        };
        let fat_size_sectors = if fat_size_16 != 0 {
            fat_size_16
        } else {
            fat_size_32
        };
        if total_sectors == 0 || fat_size_sectors == 0 {
            return Err(FatError::InvalidBootSector);
        }

        let root_dir_sectors = ((root_dir_entries as u32 * FAT_DIR_ENTRY_SIZE as u32)
            + (VIRTIO_SECTOR_SIZE as u32 - 1))
            / VIRTIO_SECTOR_SIZE as u32;
        let root_dir_first_sector = reserved_sector_count as u32
            + fat_count as u32 * fat_size_sectors;
        let data_first_sector = root_dir_first_sector + root_dir_sectors;
        if total_sectors < data_first_sector {
            return Err(FatError::InvalidBootSector);
        }

        let data_sectors = total_sectors - data_first_sector;
        let total_clusters = data_sectors / sectors_per_cluster as u32;
        let fat_type = if total_clusters < 4085 {
            return Err(FatError::UnsupportedFatType);
        } else if total_clusters < 65525 {
            FatType::Fat16
        } else {
            FatType::Fat32
        };

        let root_cluster = match fat_type {
            FatType::Fat16 => 0,
            FatType::Fat32 if root_cluster >= 2 => root_cluster,
            FatType::Fat32 => return Err(FatError::InvalidBootSector),
        };

        Ok(Self {
            device,
            partition_start_lba,
            fat_type,
            sectors_per_cluster,
            reserved_sector_count,
            root_dir_sectors,
            root_dir_first_sector,
            data_first_sector,
            root_cluster,
            total_clusters,
        })
    }

    /// Reads one file from the mounted volume into `buffer`.
    ///
    /// # Parameters
    ///
    /// - `path`: Absolute or relative path to the file inside the filesystem.
    /// - `buffer`: Output buffer that receives the complete file contents.
    pub fn read_file(
        &mut self,
        path: &str,
        buffer: &mut [u8],
    ) -> Result<usize, FatError> {
        let entry = self.find_path_entry(path)?;
        if entry.is_directory() {
            return Err(FatError::IsDirectory);
        }

        let file_size = entry.file_size as usize;
        if buffer.len() < file_size {
            return Err(FatError::BufferTooSmall);
        }

        self.read_file_contents(entry.first_cluster, entry.file_size, buffer)
    }

    /// Walks all files below the root directory and visits each file path.
    ///
    /// # Parameters
    ///
    /// - `visitor`: Callback invoked once per discovered file.
    pub fn walk_files<F>(&mut self, mut visitor: F) -> Result<(), FatError>
    where
        F: FnMut(&str, u32),
    {
        let mut path = [0u8; FAT_PATH_UTF8_BYTES];
        self.walk_directory(
            DirectoryLocation::RootDirectory,
            &mut path,
            0,
            &mut visitor,
        )
    }

    /// Resolves one filesystem path to its matching directory entry.
    ///
    /// # Parameters
    ///
    /// - `path`: Absolute or relative path inside the mounted filesystem.
    fn find_path_entry(&mut self, path: &str) -> Result<FatDirectoryEntry, FatError> {
        let mut components = path.split('/').filter(|component| !component.is_empty()).peekable();
        let mut directory = DirectoryLocation::RootDirectory;
        let mut resolved = None;

        while let Some(component) = components.next() {
            if component == "." || component == ".." {
                return Err(FatError::InvalidPath);
            }

            let entry = self.find_directory_entry(directory, component)?;
            if components.peek().is_some() {
                if !entry.is_directory() {
                    return Err(FatError::NotDirectory);
                }

                directory = DirectoryLocation::ClusterChain(entry.first_cluster);
            }

            resolved = Some(entry);
        }

        resolved.ok_or(FatError::InvalidPath)
    }

    /// Searches one directory for a path component.
    ///
    /// # Parameters
    ///
    /// - `directory`: Directory location to search.
    /// - `component`: Path component name to resolve.
    fn find_directory_entry(
        &mut self,
        directory: DirectoryLocation,
        component: &str,
    ) -> Result<FatDirectoryEntry, FatError> {
        let mut sector = [0u8; VIRTIO_SECTOR_SIZE];
        let mut long_name_state = LongNameState::new();
        let mut long_name_utf8 = [0u8; FAT_LONG_NAME_UTF8_BYTES];
        let mut short_name = [0u8; 13];

        match directory {
            DirectoryLocation::RootDirectory => match self.fat_type {
                FatType::Fat16 => {
                    let mut sector_index = 0u32;
                    while sector_index < self.root_dir_sectors {
                        self.read_relative_blocks(
                            self.root_dir_first_sector + sector_index,
                            &mut sector,
                        )?;

                        match scan_directory_sector(
                            &sector,
                            component,
                            &mut long_name_state,
                            &mut long_name_utf8,
                            &mut short_name,
                        )? {
                            DirectoryScanResult::Found(entry) => return Ok(entry),
                            DirectoryScanResult::EndOfDirectory => break,
                            DirectoryScanResult::Continue => {}
                        }

                        sector_index += 1;
                    }
                }
                FatType::Fat32 => {
                    return self.find_directory_entry(
                        DirectoryLocation::ClusterChain(self.root_cluster),
                        component,
                    );
                }
            },
            DirectoryLocation::ClusterChain(mut cluster) => {
                while self.is_valid_cluster(cluster) {
                    let first_sector = self.cluster_first_sector(cluster)?;
                    let mut sector_offset = 0u32;
                    while sector_offset < self.sectors_per_cluster as u32 {
                        self.read_relative_blocks(
                            first_sector + sector_offset,
                            &mut sector,
                        )?;

                        match scan_directory_sector(
                            &sector,
                            component,
                            &mut long_name_state,
                            &mut long_name_utf8,
                            &mut short_name,
                        )? {
                            DirectoryScanResult::Found(entry) => return Ok(entry),
                            DirectoryScanResult::EndOfDirectory => {
                                return Err(FatError::NotFound);
                            }
                            DirectoryScanResult::Continue => {}
                        }

                        sector_offset += 1;
                    }

                    cluster = self.next_cluster(cluster)?;
                    if self.is_end_of_chain(cluster) {
                        break;
                    }
                }
            }
        }

        Err(FatError::NotFound)
    }

    /// Reads one complete file cluster chain into `buffer`.
    ///
    /// # Parameters
    ///
    /// - `first_cluster`: First data cluster of the file.
    /// - `file_size`: Size of the file in bytes.
    /// - `buffer`: Output buffer that receives the file contents.
    fn read_file_contents(
        &mut self,
        first_cluster: u32,
        file_size: u32,
        buffer: &mut [u8],
    ) -> Result<usize, FatError> {
        if file_size == 0 {
            return Ok(0);
        }

        if !self.is_valid_cluster(first_cluster) {
            return Err(FatError::InvalidCluster(first_cluster));
        }

        let mut cluster = first_cluster;
        let mut written = 0usize;
        let mut remaining = file_size as usize;
        let mut tail_sector = [0u8; VIRTIO_SECTOR_SIZE];

        while remaining != 0 {
            let cluster_first_sector = self.cluster_first_sector(cluster)?;
            let full_sectors = min(
                remaining / VIRTIO_SECTOR_SIZE,
                self.sectors_per_cluster as usize,
            );

            if full_sectors != 0 {
                let bytes = full_sectors * VIRTIO_SECTOR_SIZE;
                self.read_relative_blocks_multi(
                    cluster_first_sector,
                    &mut buffer[written..written + bytes],
                )?;
                written += bytes;
                remaining -= bytes;
            }

            if remaining == 0 {
                break;
            }

            if full_sectors < self.sectors_per_cluster as usize {
                self.read_relative_blocks(
                    cluster_first_sector + full_sectors as u32,
                    &mut tail_sector,
                )?;
                let tail_len = min(remaining, VIRTIO_SECTOR_SIZE);
                buffer[written..written + tail_len]
                    .copy_from_slice(&tail_sector[..tail_len]);
                written += tail_len;
                remaining -= tail_len;
            }

            if remaining == 0 {
                break;
            }

            cluster = self.next_cluster(cluster)?;
            if self.is_end_of_chain(cluster) {
                return Err(FatError::InvalidCluster(cluster));
            }
        }

        Ok(written)
    }

    /// Recursively walks all files contained in `directory`.
    ///
    /// # Parameters
    ///
    /// - `directory`: Directory location to enumerate.
    /// - `path`: Mutable path buffer reused across recursion.
    /// - `path_len`: Number of bytes currently stored in `path`.
    /// - `visitor`: Callback invoked once per discovered file.
    fn walk_directory<F>(
        &mut self,
        directory: DirectoryLocation,
        path: &mut [u8; FAT_PATH_UTF8_BYTES],
        path_len: usize,
        visitor: &mut F,
    ) -> Result<(), FatError>
    where
        F: FnMut(&str, u32),
    {
        let mut sector = [0u8; VIRTIO_SECTOR_SIZE];
        let mut long_name_state = LongNameState::new();
        let mut long_name_utf8 = [0u8; FAT_LONG_NAME_UTF8_BYTES];
        let mut short_name = [0u8; 13];

        match directory {
            DirectoryLocation::RootDirectory => match self.fat_type {
                FatType::Fat16 => {
                    let mut sector_index = 0u32;
                    while sector_index < self.root_dir_sectors {
                        self.read_relative_blocks(
                            self.root_dir_first_sector + sector_index,
                            &mut sector,
                        )?;

                        if scan_directory_sector_entries(
                            &sector,
                            &mut long_name_state,
                            &mut long_name_utf8,
                            &mut short_name,
                            &mut |name, entry| {
                                self.visit_walk_entry(
                                    name,
                                    entry,
                                    path,
                                    path_len,
                                    visitor,
                                )
                            },
                        )? {
                            break;
                        }

                        sector_index += 1;
                    }
                }
                FatType::Fat32 => {
                    self.walk_directory(
                        DirectoryLocation::ClusterChain(self.root_cluster),
                        path,
                        path_len,
                        visitor,
                    )?;
                }
            },
            DirectoryLocation::ClusterChain(mut cluster) => {
                while self.is_valid_cluster(cluster) {
                    let first_sector = self.cluster_first_sector(cluster)?;
                    let mut sector_offset = 0u32;
                    while sector_offset < self.sectors_per_cluster as u32 {
                        self.read_relative_blocks(
                            first_sector + sector_offset,
                            &mut sector,
                        )?;

                        if scan_directory_sector_entries(
                            &sector,
                            &mut long_name_state,
                            &mut long_name_utf8,
                            &mut short_name,
                            &mut |name, entry| {
                                self.visit_walk_entry(
                                    name,
                                    entry,
                                    path,
                                    path_len,
                                    visitor,
                                )
                            },
                        )? {
                            return Ok(());
                        }

                        sector_offset += 1;
                    }

                    cluster = self.next_cluster(cluster)?;
                    if self.is_end_of_chain(cluster) {
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// Processes one directory entry discovered during recursive walking.
    ///
    /// # Parameters
    ///
    /// - `name`: Decoded entry name.
    /// - `entry`: Parsed directory entry metadata.
    /// - `path`: Mutable path buffer reused across recursion.
    /// - `path_len`: Number of bytes currently stored in `path`.
    /// - `visitor`: Callback invoked once per discovered file.
    fn visit_walk_entry<F>(
        &mut self,
        name: &str,
        entry: FatDirectoryEntry,
        path: &mut [u8; FAT_PATH_UTF8_BYTES],
        path_len: usize,
        visitor: &mut F,
    ) -> Result<(), FatError>
    where
        F: FnMut(&str, u32),
    {
        if name == "." || name == ".." {
            return Ok(());
        }

        let full_path_len = append_path_component(path, path_len, name)?;
        let full_path = unsafe { str::from_utf8_unchecked(&path[..full_path_len]) };

        if entry.is_directory() {
            self.walk_directory(
                DirectoryLocation::ClusterChain(entry.first_cluster),
                path,
                full_path_len,
                visitor,
            )
        } else {
            visitor(full_path, entry.file_size);
            Ok(())
        }
    }

    /// Reads one partition-relative sector into `buffer`.
    ///
    /// # Parameters
    ///
    /// - `relative_sector`: Partition-relative sector index to read.
    /// - `buffer`: One-sector destination buffer.
    fn read_relative_blocks(
        &mut self,
        relative_sector: u32,
        buffer: &mut [u8; VIRTIO_SECTOR_SIZE],
    ) -> Result<(), FatError> {
        self.device
            .read_blocks(self.partition_start_lba + relative_sector as u64, buffer)
            .map_err(FatError::from)
    }

    /// Reads multiple contiguous partition-relative sectors into `buffer`.
    ///
    /// # Parameters
    ///
    /// - `relative_sector`: Partition-relative first sector to read.
    /// - `buffer`: Destination buffer sized to a whole number of sectors.
    fn read_relative_blocks_multi(
        &mut self,
        relative_sector: u32,
        buffer: &mut [u8],
    ) -> Result<(), FatError> {
        self.device
            .read_blocks(self.partition_start_lba + relative_sector as u64, buffer)
            .map_err(FatError::from)
    }

    /// Returns the first sector of one data cluster.
    ///
    /// # Parameters
    ///
    /// - `cluster`: Cluster number whose first sector is requested.
    fn cluster_first_sector(&self, cluster: u32) -> Result<u32, FatError> {
        if !self.is_valid_cluster(cluster) {
            return Err(FatError::InvalidCluster(cluster));
        }

        Ok(self.data_first_sector
            + (cluster - 2) * self.sectors_per_cluster as u32)
    }

    /// Returns `true` when `cluster` is a valid data cluster number.
    ///
    /// # Parameters
    ///
    /// - `cluster`: Cluster number to validate.
    fn is_valid_cluster(&self, cluster: u32) -> bool {
        cluster >= 2 && cluster < self.total_clusters + 2
    }

    /// Returns `true` when `cluster` is an end-of-chain marker.
    ///
    /// # Parameters
    ///
    /// - `cluster`: FAT entry value to classify.
    fn is_end_of_chain(&self, cluster: u32) -> bool {
        match self.fat_type {
            FatType::Fat16 => cluster >= FAT16_END_OF_CHAIN,
            FatType::Fat32 => cluster >= FAT32_END_OF_CHAIN,
        }
    }

    /// Returns the next cluster in a FAT chain.
    ///
    /// # Parameters
    ///
    /// - `cluster`: Current cluster number whose FAT entry should be read.
    fn next_cluster(&mut self, cluster: u32) -> Result<u32, FatError> {
        if !self.is_valid_cluster(cluster) {
            return Err(FatError::InvalidCluster(cluster));
        }

        let entry_size = match self.fat_type {
            FatType::Fat16 => 2u32,
            FatType::Fat32 => 4u32,
        };
        let fat_offset = cluster * entry_size;
        let fat_sector = self.reserved_sector_count as u32
            + fat_offset / VIRTIO_SECTOR_SIZE as u32;
        let entry_offset = fat_offset as usize % VIRTIO_SECTOR_SIZE;
        let mut sector = [0u8; VIRTIO_SECTOR_SIZE];
        self.read_relative_blocks(fat_sector, &mut sector)?;

        let next = match self.fat_type {
            FatType::Fat16 => read_u16(&sector, entry_offset)
                .ok_or(FatError::InvalidBootSector)? as u32,
            FatType::Fat32 => read_u32(&sector, entry_offset)
                .ok_or(FatError::InvalidBootSector)? & 0x0fff_ffff,
        };

        match self.fat_type {
            FatType::Fat16 if next == FAT16_BAD_CLUSTER => {
                Err(FatError::InvalidCluster(next))
            }
            FatType::Fat32 if next == FAT32_BAD_CLUSTER => {
                Err(FatError::InvalidCluster(next))
            }
            _ => Ok(next),
        }
    }
}

/// Scans one directory sector for `component`.
///
/// # Parameters
///
/// - `sector`: Raw directory sector bytes to scan.
/// - `component`: Path component name to search for.
/// - `long_name_state`: State used to assemble VFAT long names.
/// - `long_name_utf8`: Scratch buffer used to decode long names.
/// - `short_name`: Scratch buffer used to format short names.
fn scan_directory_sector(
    sector: &[u8; VIRTIO_SECTOR_SIZE],
    component: &str,
    long_name_state: &mut LongNameState,
    long_name_utf8: &mut [u8; FAT_LONG_NAME_UTF8_BYTES],
    short_name: &mut [u8; 13],
) -> Result<DirectoryScanResult, FatError> {
    let mut offset = 0usize;
    while offset < sector.len() {
        let entry = sector_entry(sector, offset);
        let first_byte = entry[0];

        if first_byte == 0x00 {
            long_name_state.clear();
            return Ok(DirectoryScanResult::EndOfDirectory);
        }

        if first_byte == 0xe5 {
            long_name_state.clear();
            offset += FAT_DIR_ENTRY_SIZE;
            continue;
        }

        let attributes = entry[11];
        if attributes == FAT_ATTRIBUTE_LONG_NAME {
            let _ = long_name_state.push(&entry);
            offset += FAT_DIR_ENTRY_SIZE;
            continue;
        }

        if (attributes & FAT_ATTRIBUTE_VOLUME_ID) != 0 {
            long_name_state.clear();
            offset += FAT_DIR_ENTRY_SIZE;
            continue;
        }

        let matches = if long_name_state.matches_entry(&entry) {
            long_name_state
                .to_utf8(long_name_utf8)?
                .eq_ignore_ascii_case(component)
        } else {
            decode_short_name(&entry, short_name).eq_ignore_ascii_case(component)
        };

        let directory_entry = parse_directory_entry(&entry)?;
        long_name_state.clear();
        if matches {
            return Ok(DirectoryScanResult::Found(directory_entry));
        }

        offset += FAT_DIR_ENTRY_SIZE;
    }

    Ok(DirectoryScanResult::Continue)
}

/// Visits all regular directory entries stored in one directory sector.
///
/// # Parameters
///
/// - `sector`: Raw directory sector bytes to scan.
/// - `long_name_state`: State used to assemble VFAT long names.
/// - `long_name_utf8`: Scratch buffer used to decode long names.
/// - `short_name`: Scratch buffer used to format short names.
/// - `visitor`: Callback invoked for each regular directory entry.
fn scan_directory_sector_entries<F>(
    sector: &[u8; VIRTIO_SECTOR_SIZE],
    long_name_state: &mut LongNameState,
    long_name_utf8: &mut [u8; FAT_LONG_NAME_UTF8_BYTES],
    short_name: &mut [u8; 13],
    visitor: &mut F,
) -> Result<bool, FatError>
where
    F: FnMut(&str, FatDirectoryEntry) -> Result<(), FatError>,
{
    let mut offset = 0usize;
    while offset < sector.len() {
        let entry = sector_entry(sector, offset);
        let first_byte = entry[0];

        if first_byte == 0x00 {
            long_name_state.clear();
            return Ok(true);
        }

        if first_byte == 0xe5 {
            long_name_state.clear();
            offset += FAT_DIR_ENTRY_SIZE;
            continue;
        }

        let attributes = entry[11];
        if attributes == FAT_ATTRIBUTE_LONG_NAME {
            let _ = long_name_state.push(&entry);
            offset += FAT_DIR_ENTRY_SIZE;
            continue;
        }

        if (attributes & FAT_ATTRIBUTE_VOLUME_ID) != 0 {
            long_name_state.clear();
            offset += FAT_DIR_ENTRY_SIZE;
            continue;
        }

        let directory_entry = parse_directory_entry(&entry)?;
        if long_name_state.matches_entry(&entry) {
            let name = long_name_state.to_utf8(long_name_utf8)?;
            visitor(name, directory_entry)?;
        } else {
            let name = decode_short_name(&entry, short_name);
            visitor(name, directory_entry)?;
        }

        long_name_state.clear();
        offset += FAT_DIR_ENTRY_SIZE;
    }

    Ok(false)
}

/// Appends one path component to `path` and returns the resulting length.
///
/// # Parameters
///
/// - `path`: Path buffer that receives the appended component.
/// - `path_len`: Number of bytes already present in `path`.
/// - `component`: Path component to append.
fn append_path_component(
    path: &mut [u8; FAT_PATH_UTF8_BYTES],
    path_len: usize,
    component: &str,
) -> Result<usize, FatError> {
    let total_len = path_len + 1 + component.len();
    if total_len > path.len() {
        return Err(FatError::NameTooLong);
    }

    path[path_len] = b'/';
    path[path_len + 1..total_len].copy_from_slice(component.as_bytes());
    Ok(total_len)
}

/// Returns one raw 32-byte directory entry from `sector`.
///
/// # Parameters
///
/// - `sector`: Raw directory sector bytes.
/// - `offset`: Byte offset of the desired directory entry.
fn sector_entry(
    sector: &[u8; VIRTIO_SECTOR_SIZE],
    offset: usize,
) -> [u8; FAT_DIR_ENTRY_SIZE] {
    let mut entry = [0u8; FAT_DIR_ENTRY_SIZE];
    entry.copy_from_slice(&sector[offset..offset + FAT_DIR_ENTRY_SIZE]);
    entry
}

/// Parses one regular FAT directory entry.
///
/// # Parameters
///
/// - `entry`: Raw 32-byte regular directory entry.
fn parse_directory_entry(
    entry: &[u8; FAT_DIR_ENTRY_SIZE],
) -> Result<FatDirectoryEntry, FatError> {
    let cluster_high = read_u16(entry, 20).ok_or(FatError::InvalidBootSector)?;
    let cluster_low = read_u16(entry, 26).ok_or(FatError::InvalidBootSector)?;

    Ok(FatDirectoryEntry {
        attributes: entry[11],
        first_cluster: ((cluster_high as u32) << 16) | cluster_low as u32,
        file_size: read_u32(entry, 28).ok_or(FatError::InvalidBootSector)?,
    })
}

/// Decodes one FAT short name into a printable string.
///
/// # Parameters
///
/// - `entry`: Raw 32-byte directory entry containing the short name.
/// - `buffer`: Output buffer that receives the formatted short name.
fn decode_short_name<'a>(
    entry: &[u8; FAT_DIR_ENTRY_SIZE],
    buffer: &'a mut [u8; 13],
) -> &'a str {
    let mut output = 0usize;

    for byte in &entry[0..8] {
        if *byte == b' ' {
            break;
        }

        buffer[output] = if *byte == 0x05 { 0xe5 } else { *byte };
        output += 1;
    }

    let extension_start = output;
    let has_extension = entry[8..11].iter().any(|byte| *byte != b' ');
    if has_extension {
        buffer[output] = b'.';
        output += 1;

        for byte in &entry[8..11] {
            if *byte == b' ' {
                break;
            }

            buffer[output] = *byte;
            output += 1;
        }
    }

    if output == extension_start && !has_extension {
        buffer[0] = b'-';
        output = 1;
    }

    unsafe { str::from_utf8_unchecked(&buffer[..output]) }
}

/// Copies the UTF-16 code units stored by one VFAT long-name entry.
///
/// # Parameters
///
/// - `entry`: Raw 32-byte long-name directory entry.
/// - `output`: Slice that receives the 13 UTF-16 code units from `entry`.
fn copy_lfn_code_units(
    entry: &[u8; FAT_DIR_ENTRY_SIZE],
    output: &mut [u16],
) {
    let positions = [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
    let mut index = 0usize;
    while index < positions.len() {
        let offset = positions[index];
        output[index] = u16::from_le_bytes([entry[offset], entry[offset + 1]]);
        index += 1;
    }
}

/// Computes the short-name checksum referenced by a VFAT long name.
///
/// # Parameters
///
/// - `short_name`: The 11-byte FAT short-name field.
fn short_name_checksum(short_name: &[u8]) -> u8 {
    let mut checksum = 0u8;
    let mut index = 0usize;
    while index < short_name.len() {
        checksum = ((checksum & 1) << 7)
            .wrapping_add(checksum >> 1)
            .wrapping_add(short_name[index]);
        index += 1;
    }

    checksum
}

/// Reads one little-endian `u16` from `bytes`.
///
/// # Parameters
///
/// - `bytes`: Byte slice containing the encoded value.
/// - `offset`: Starting byte offset of the value.
fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let data = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([data[0], data[1]]))
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