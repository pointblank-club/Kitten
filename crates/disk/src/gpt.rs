//! Reading and rewriting the GUID partition table.
//!
//! The table lives in two regions: the protective MBR, primary header and entries before the
//! first usable sector, and the backup entries and header after the last one. Those regions are
//! held in memory, edited there with `gptman`, and written back sector by sector.

use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use bootshift_core::layout::{Extent, Guid, Partition};
use bootshift_core::units::{Lba, Sectors};
use gptman::{GPT, GPTHeader, GPTPartitionEntry};

use crate::device::{BlockDevice, DiskError};

/// Larger GPT regions than this mean a damaged header, not a real table.
const MAX_REGION_LEN: u64 = 16 << 20;

#[derive(Debug, thiserror::Error)]
pub enum GptError {
    #[error(transparent)]
    Disk(#[from] DiskError),
    #[error("the partition table can't be read: {0}")]
    Unreadable(#[from] gptman::Error),
    #[error("the partition table's header describes an impossible disk")]
    BadGeometry,
    #[error("the table holds partition {0}, which the new layout doesn't describe")]
    UnknownPartition(Guid),
    #[error("partition {0} would change its slot or type")]
    IdentityChanged(Guid),
    #[error("slot {0} is missing or already holds another partition")]
    SlotUnavailable(u32),
    #[error("partition {0} has no space")]
    EmptyPartition(Guid),
}

/// One used entry of the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GptEntry {
    pub number: u32,
    pub type_guid: Guid,
    pub unique_guid: Guid,
    pub name: String,
    pub attributes: u64,
    pub extent: Extent,
}

/// The two GPT regions of a disk, held in memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GptImage {
    sector_size: u64,
    disk_len: u64,
    head: Vec<u8>,
    tail_offset: u64,
    tail: Vec<u8>,
}

impl GptImage {
    /// Reads the table, falling back to the backup copy if the primary one is damaged.
    pub fn read<D: BlockDevice + ?Sized>(device: &D) -> Result<GptImage, GptError> {
        let sector_size = u64::from(device.logical_block_size());
        let disk_len = device.len_bytes();
        let header = read_any_header(device, sector_size, disk_len)?;

        let head_len = header
            .first_usable_lba
            .checked_mul(sector_size)
            .ok_or(GptError::BadGeometry)?;
        let tail_offset = header
            .last_usable_lba
            .checked_add(1)
            .and_then(|lba| lba.checked_mul(sector_size))
            .ok_or(GptError::BadGeometry)?;
        let tail_len = disk_len
            .checked_sub(tail_offset)
            .ok_or(GptError::BadGeometry)?;
        if head_len > MAX_REGION_LEN || tail_len > MAX_REGION_LEN || head_len > tail_offset {
            return Err(GptError::BadGeometry);
        }

        let mut head = vec![0; head_len as usize];
        let mut tail = vec![0; tail_len as usize];
        device.read_at(0, &mut head)?;
        device.read_at(tail_offset, &mut tail)?;

        let image = GptImage {
            sector_size,
            disk_len,
            head,
            tail_offset,
            tail,
        };
        image.parse()?;
        Ok(image)
    }

    /// The disk's own GUID, which identifies it even if its device name changes.
    pub fn disk_guid(&self) -> Result<Guid, GptError> {
        Ok(Guid(self.parse()?.header.disk_guid))
    }

    pub fn sector_size(&self) -> u32 {
        self.sector_size as u32
    }

    /// The sectors partitions may occupy.
    pub fn usable(&self) -> Extent {
        let first = self.head.len() as u64 / self.sector_size;
        let end = self.tail_offset / self.sector_size;
        Extent::new(Lba(first), Sectors(end - first))
    }

    pub fn entries(&self) -> Result<Vec<GptEntry>, GptError> {
        let gpt = self.parse()?;
        let total_sectors = self.disk_len / self.sector_size;
        gpt.iter()
            .filter(|(_, entry)| entry.is_used())
            .map(|(number, entry)| {
                // The LBAs come straight from the on-disk table, which gptman does not bound on
                // read. This is reached by the password-less inspect and the PID-1 recovery path, so
                // refuse a reversed or out-of-range entry rather than letting the length arithmetic
                // wrap (and abort under release overflow-checks).
                if entry.ending_lba < entry.starting_lba || entry.ending_lba >= total_sectors {
                    return Err(GptError::BadGeometry);
                }
                let sectors = entry
                    .ending_lba
                    .checked_sub(entry.starting_lba)
                    .and_then(|span| span.checked_add(1))
                    .ok_or(GptError::BadGeometry)?;
                Ok(GptEntry {
                    number,
                    type_guid: Guid(entry.partition_type_guid),
                    unique_guid: Guid(entry.unique_partition_guid),
                    name: entry.partition_name.as_str().to_string(),
                    attributes: entry.attribute_bits,
                    extent: Extent::new(Lba(entry.starting_lba), Sectors(sectors)),
                })
            })
            .collect()
    }

    /// The table with each partition at its extent in `partitions`.
    ///
    /// Every partition keeps its slot, GUIDs, type, name and attributes. A partition missing
    /// from `partitions` is removed only if listed in `removable`; anything else the table
    /// holds but `partitions` doesn't describe is refused, so no entry is ever dropped by
    /// mistake. Partitions not yet in the table are added in their own slot.
    pub fn with_partitions(
        &self,
        partitions: &[Partition],
        removable: &[Guid],
    ) -> Result<GptImage, GptError> {
        let mut gpt = self.parse()?;
        let mut present = Vec::new();

        for (number, entry) in gpt.iter_mut() {
            if entry.is_unused() {
                continue;
            }
            let guid = Guid(entry.unique_partition_guid);
            match partitions.iter().find(|p| p.unique_guid == guid) {
                Some(partition) => {
                    if partition.number != number
                        || partition.type_guid != Guid(entry.partition_type_guid)
                    {
                        return Err(GptError::IdentityChanged(guid));
                    }
                    set_extent(entry, partition)?;
                    present.push(guid);
                }
                None if removable.contains(&guid) => *entry = GPTPartitionEntry::empty(),
                None => return Err(GptError::UnknownPartition(guid)),
            }
        }

        for partition in partitions
            .iter()
            .filter(|p| !present.contains(&p.unique_guid))
        {
            let (_, slot) = gpt
                .iter_mut()
                .find(|(number, entry)| *number == partition.number && entry.is_unused())
                .ok_or(GptError::SlotUnavailable(partition.number))?;
            *slot = GPTPartitionEntry {
                partition_type_guid: partition.type_guid.0,
                unique_partition_guid: partition.unique_guid.0,
                starting_lba: 0,
                ending_lba: 0,
                attribute_bits: partition.attributes,
                partition_name: partition.name.as_str().into(),
            };
            set_extent(slot, partition)?;
        }

        let mut image = self.clone();
        gpt.write_into(&mut SparseImage::new(&mut image))?;
        Ok(image)
    }

    /// Writes both copies of a table made by [`GptImage::with_partitions`]: the backup copy,
    /// a flush, then the primary entries and header, and a flush. At every moment at least one
    /// copy is complete. Only the table's own sectors are written, never the protective MBR or
    /// the gap before the first partition.
    ///
    /// Rebuilding the same layout from whichever copy a crash left valid, old or new, gives the
    /// same table, so a torn write is resumed by simply doing it again.
    pub fn write<D: BlockDevice + ?Sized>(&self, device: &mut D) -> Result<(), GptError> {
        device.write_at(self.tail_offset, &self.tail)?;
        device.flush()?;
        for range in self.primary_ranges()? {
            device.write_at(range.start as u64, &self.head[range])?;
        }
        device.flush()?;
        Ok(())
    }

    /// The primary entry array and header, as byte ranges of the head region.
    fn primary_ranges(&self) -> Result<[Range<usize>; 2], GptError> {
        let sector = self.sector_size;
        let header_bytes = self
            .head
            .get(sector as usize..2 * sector as usize)
            .ok_or(GptError::BadGeometry)?;
        let header = GPTHeader::read_from(&mut Cursor::new(header_bytes))?;
        let entries_start = header.partition_entry_lba * sector;
        let entries_len = u64::from(header.number_of_partition_entries)
            * u64::from(header.size_of_partition_entry);
        let entries_end = (entries_start + entries_len).next_multiple_of(sector);
        if entries_start < 2 * sector || entries_end > self.head.len() as u64 {
            return Err(GptError::BadGeometry);
        }
        Ok([
            entries_start as usize..entries_end as usize,
            sector as usize..2 * sector as usize,
        ])
    }

    fn parse(&self) -> Result<GPT, GptError> {
        let mut image = self.clone();
        Ok(GPT::read_from(
            &mut SparseImage::new(&mut image),
            self.sector_size,
        )?)
    }
}

fn read_any_header<D: BlockDevice + ?Sized>(
    device: &D,
    sector_size: u64,
    disk_len: u64,
) -> Result<GPTHeader, GptError> {
    let last_sector = disk_len
        .checked_sub(sector_size)
        .ok_or(GptError::BadGeometry)?;
    let mut sector = vec![0; sector_size as usize];
    device.read_at(sector_size, &mut sector)?;
    match GPTHeader::read_from(&mut Cursor::new(&sector)) {
        Ok(header) => Ok(header),
        Err(_) => {
            device.read_at(last_sector, &mut sector)?;
            Ok(GPTHeader::read_from(&mut Cursor::new(&sector))?)
        }
    }
}

fn set_extent(entry: &mut GPTPartitionEntry, partition: &Partition) -> Result<(), GptError> {
    let Extent { start, len } = partition.extent;
    if len == Sectors(0) {
        return Err(GptError::EmptyPartition(partition.unique_guid));
    }
    entry.starting_lba = start.0;
    entry.ending_lba = start.0 + (len.0 - 1);
    Ok(())
}

/// A whole-disk view for `gptman`: the GPT regions are real, the space between reads as zeros
/// and can't be written.
struct SparseImage<'a> {
    image: &'a mut GptImage,
    position: u64,
}

impl<'a> SparseImage<'a> {
    fn new(image: &'a mut GptImage) -> Self {
        Self { image, position: 0 }
    }

    /// The region holding the current position, the offset into it, and how many bytes of it
    /// remain. `None` means the gap between the regions, with that many bytes left in the gap.
    fn locate(&mut self) -> (Option<(&mut Vec<u8>, usize)>, u64) {
        let position = self.position;
        let head_len = self.image.head.len() as u64;
        if position < head_len {
            (
                Some((&mut self.image.head, position as usize)),
                head_len - position,
            )
        } else if position >= self.image.tail_offset {
            let offset = position - self.image.tail_offset;
            let remaining = (self.image.tail.len() as u64).saturating_sub(offset);
            (Some((&mut self.image.tail, offset as usize)), remaining)
        } else {
            (None, self.image.tail_offset - position)
        }
    }
}

impl Read for SparseImage<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (region, remaining) = self.locate();
        let len = (remaining.min(buf.len() as u64)) as usize;
        match region {
            Some((bytes, offset)) => buf[..len].copy_from_slice(&bytes[offset..offset + len]),
            None => buf[..len].fill(0),
        }
        self.position += len as u64;
        Ok(len)
    }
}

impl Write for SparseImage<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let (region, remaining) = self.locate();
        let Some((bytes, offset)) = region else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "write outside the partition table regions",
            ));
        };
        let len = (remaining.min(data.len() as u64)) as usize;
        if len == 0 && !data.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past the end of the disk",
            ));
        }
        bytes[offset..offset + len].copy_from_slice(&data[..len]);
        self.position += len as u64;
        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for SparseImage<'_> {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "seek out of range");
        let position = match target {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.image.disk_len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
        };
        self.position = position.ok_or_else(invalid)?;
        Ok(self.position)
    }
}
