//! Partition table abstractions shared by GPT and future MBR support.
//!
//! This module defines a minimal interface for enumerating partitions without
//! coupling callers to one on-disk partition-table format.

/// Describes one partition entry exposed by a partition table.
pub trait PartitionEntry {
    /// Returns `true` when this entry describes an allocated partition.
    fn is_present(&self) -> bool;

    /// Returns the first logical block address owned by the partition.
    fn first_lba(&self) -> u64;

    /// Returns the last logical block address owned by the partition.
    fn last_lba(&self) -> u64;

    /// Returns the number of sectors covered by the partition.
    fn sector_count(&self) -> u64 {
        self.last_lba() - self.first_lba() + 1
    }

    /// Returns `true` when the partition is marked bootable.
    fn bootable(&self) -> bool;

    /// Returns `true` when the partition is an EFI system partition.
    fn is_efi_system_partition(&self) -> bool {
        false
    }

    /// Formats the partition label into the provided scratch buffer.
    ///
    /// # Parameters
    ///
    /// - `buffer`: Scratch buffer that receives a printable partition label.
    fn label<'a>(&self, buffer: &'a mut [u8; 72]) -> &'a str;

    /// Formats the partition type into the provided scratch buffer.
    ///
    /// # Parameters
    ///
    /// - `buffer`: Scratch buffer that receives a printable partition type.
    fn partition_type<'a>(&self, buffer: &'a mut [u8; 36]) -> &'a str;
}

/// Enumerates partitions from one on-disk partition-table format.
pub trait PartitionTable {
    /// Concrete partition entry type yielded by this table.
    type Entry: PartitionEntry;

    /// Returns the total number of table slots that may contain partitions.
    fn partition_count(&self) -> u32;

    /// Reads one partition-table entry by zero-based index.
    ///
    /// # Parameters
    ///
    /// - `index`: Zero-based partition entry index to decode.
    fn partition(&mut self, index: u32) -> Option<Self::Entry>;
}