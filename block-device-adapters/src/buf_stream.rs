use aligned::Aligned;
use block_device_driver::{slice_to_blocks, slice_to_blocks_mut, BlockDevice};
use embedded_io_async::{ErrorKind, Read, Seek, SeekFrom, Write};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub enum BufStreamError<T> {
    Io(T),
    /// The seek target is before the start of the device, or above `u64::MAX`.
    InvalidSeek,
}

impl<T> From<T> for BufStreamError<T> {
    fn from(t: T) -> Self {
        BufStreamError::Io(t)
    }
}

impl<T: core::fmt::Debug> core::fmt::Display for BufStreamError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BufStreamError::Io(e) => write!(f, "IO error: {e:?}"),
            BufStreamError::InvalidSeek => write!(f, "Invalid seek"),
        }
    }
}

impl<T: core::fmt::Debug> core::error::Error for BufStreamError<T> {}

impl<T: core::fmt::Debug> embedded_io_async::Error for BufStreamError<T> {
    fn kind(&self) -> ErrorKind {
        match self {
            BufStreamError::Io(_) => ErrorKind::Other,
            BufStreamError::InvalidSeek => ErrorKind::InvalidInput,
        }
    }
}

/// A Stream wrapper for accessing a stream in block sized chunks.
///
/// [`BufStream<T, const SIZE: usize, B>`](BufStream) can be initialized with the following parameters.
///
/// - `T`: The inner stream.
/// - `SIZE`: The size of the block, this dictates the size of the internal buffers.
/// - `B`: The [`Slot`]s that hold blocks in memory: by default one slot inside the stream ([`BufStream::new`]).
///   [`BufStream::with_slots`] takes an array of slots, or a reference to slots kept elsewhere, such as in a
///   `static`. A stream that is part of an `async` task's state is moved and copied with it; slots kept
///   elsewhere stay out of those copies.
///
/// If the `buf` provided to either [`Read::read`] or [`Write::write`] meets the following conditions the `buf`
/// will be used directly instead of an intermediate buffer to avoid unnecessary copies:
///
/// - `buf.len()` is a multiple of block size
/// - `buf` has the same alignment as the internal buffers
/// - The byte address of the inner device is aligned to a block size.
///
/// [`BufStream`] implements the [`embedded_io_async`] traits, and implicitly handles the RMW (Read, Modify,
/// Write) cycle for you.
///
/// With one slot, a caller that alternates between two blocks makes every access write one block back and read
/// the other. A FAT filesystem with a mirrored table does that for each cluster it allocates or frees, so two
/// slots let it change a whole table sector, and its mirror, with one read and one write each. A modified block
/// stays in memory until its last byte is written, its slot is needed for another block, or [`Write::flush`].
pub struct BufStream<
    T: BlockDevice<SIZE>,
    const SIZE: usize,
    B = [Slot<<T as BlockDevice<SIZE>>::Align, SIZE>; 1],
> {
    inner: T,
    slots: B,
    /// Counts slot uses, so the least recently used slot can be found.
    uses: u32,
    current_offset: u64,
}

/// Marks a slot that holds no block.
const NO_BLOCK: u32 = u32::MAX;

/// One block kept in memory by a [`BufStream`].
pub struct Slot<ALIGN, const SIZE: usize> {
    buffer: Aligned<ALIGN, [u8; SIZE]>,
    block: u32,
    dirty: bool,
    /// Value of [`BufStream::uses`] when the slot was last used.
    used: u32,
}

impl<ALIGN: aligned::Alignment, const SIZE: usize> Slot<ALIGN, SIZE> {
    /// An empty slot.
    pub const fn new() -> Self {
        Self {
            buffer: Aligned([0; SIZE]),
            block: NO_BLOCK,
            dirty: false,
            used: 0,
        }
    }
}

impl<ALIGN: aligned::Alignment, const SIZE: usize> Default for Slot<ALIGN, SIZE> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: BlockDevice<SIZE>, const SIZE: usize> BufStream<T, SIZE> {
    /// Create a new [`BufStream`] around a hardware block device, with one slot inside the stream.
    pub fn new(inner: T) -> Self {
        Self::with_slots(inner, [Slot::new()])
    }
}

impl<T: BlockDevice<SIZE>, const SIZE: usize, B: AsMut<[Slot<T::Align, SIZE>]>>
    BufStream<T, SIZE, B>
{
    const ALIGN: usize = core::mem::align_of::<Aligned<T::Align, [u8; SIZE]>>();

    /// Create a new [`BufStream`] around a hardware block device that keeps blocks in `slots`.
    ///
    /// The slots are emptied first: what an earlier stream left in them is not used.
    ///
    /// # Panics
    ///
    /// Panics if `slots` is empty.
    pub fn with_slots(inner: T, mut slots: B) -> Self {
        assert!(
            !slots.as_mut().is_empty(),
            "BufStream needs at least one slot"
        );
        for slot in slots.as_mut() {
            slot.block = NO_BLOCK;
            slot.dirty = false;
            slot.used = 0;
        }
        Self {
            inner,
            slots,
            uses: 0,
            current_offset: 0,
        }
    }

    /// Returns inner object.
    pub fn into_inner(self) -> T {
        self.inner
    }

    #[inline]
    fn pointer_block_start_addr(&self) -> u64 {
        self.pointer_block_start() as u64 * SIZE as u64
    }

    #[inline]
    fn pointer_block_start(&self) -> u32 {
        (self.current_offset / SIZE as u64)
            .try_into()
            .expect("Block larger than 2TB")
    }

    /// Writes the slot's block to the device if it was modified.
    async fn flush_slot(&mut self, index: usize) -> Result<(), T::Error> {
        let slot = &mut self.slots.as_mut()[index];
        if slot.dirty {
            // Note, alignment of internal buffer is guarenteed at compile time so we don't have to check it here
            self.inner
                .write(slot.block, slice_to_blocks(&slot.buffer[..]))
                .await?;
            // Only a completed write cleans the buffer; after a failure the next flush retries it.
            slot.dirty = false;
        }
        Ok(())
    }

    /// Writes every modified block to the device.
    async fn flush(&mut self) -> Result<(), T::Error> {
        for index in 0..self.slot_count() {
            self.flush_slot(index).await?;
        }
        Ok(())
    }

    /// Returns the slot that holds the block under the pointer, reading the block first if no slot has it.
    async fn load(&mut self) -> Result<usize, T::Error> {
        let block = self.pointer_block_start();
        self.uses = self.uses.wrapping_add(1);
        let uses = self.uses;
        let slots = self.slots.as_mut();
        let index = match slots.iter().position(|slot| slot.block == block) {
            Some(index) => index,
            None => {
                // An empty slot if there is one, else the slot unused for the longest.
                let index = slots
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, slot)| (slot.block == NO_BLOCK, uses.wrapping_sub(slot.used)))
                    .map(|(index, _)| index)
                    .expect("at least one slot");
                // The slot may hold modified data of another block; write it to the device first.
                self.flush_slot(index).await?;
                // A failed read can leave partial data in the buffer, so it must not stay labelled as the old
                // block.
                let slot = &mut self.slots.as_mut()[index];
                slot.block = NO_BLOCK;
                self.inner
                    .read(block, slice_to_blocks_mut(&mut slot.buffer[..]))
                    .await?;
                slot.block = block;
                index
            }
        };
        self.slots.as_mut()[index].used = uses;
        Ok(index)
    }

    /// Returns true if the slot's block is in the `len` bytes that start at the current block.
    fn in_range(&mut self, index: usize, len: usize) -> bool {
        let start = u64::from(self.pointer_block_start());
        let end = start + (len / SIZE) as u64;
        (start..end).contains(&u64::from(self.slots.as_mut()[index].block))
    }

    fn slot_count(&mut self) -> usize {
        self.slots.as_mut().len()
    }
}

impl<T: BlockDevice<SIZE>, const SIZE: usize, B> embedded_io_async::ErrorType
    for BufStream<T, SIZE, B>
{
    type Error = BufStreamError<T::Error>;
}

impl<T: BlockDevice<SIZE>, const SIZE: usize, B: AsMut<[Slot<T::Align, SIZE>]>> Read
    for BufStream<T, SIZE, B>
{
    async fn read(&mut self, mut buf: &mut [u8]) -> Result<usize, Self::Error> {
        // A zero-length read must be a no-op: forwarding it would otherwise
        // satisfy the block-aligned fast path below and issue a 0-block device
        // transfer (which some block devices, e.g. STM32 SDMMC DMA, reject).
        if buf.is_empty() {
            return Ok(0);
        }
        let mut total = 0;
        let target = buf.len();
        loop {
            let bytes_read = if buf.len() % SIZE == 0
                && buf.as_ptr().cast::<u8>() as usize % Self::ALIGN == 0
                && self.current_offset % SIZE as u64 == 0
            {
                // If the provided buffer has a suitable length and alignment _and_ the read head is on a block boundary, use it directly
                // The device must have the data of a modified block in memory before we read it.
                for index in 0..self.slot_count() {
                    if self.in_range(index, buf.len()) {
                        self.flush_slot(index).await?;
                    }
                }
                let block = self.pointer_block_start();
                self.inner.read(block, slice_to_blocks_mut(buf)).await?;

                buf.len()
            } else {
                let block_start = self.pointer_block_start_addr();
                let block_end = block_start + SIZE as u64;
                trace!(
                    "offset {}, block_start {}, block_end {}",
                    self.current_offset,
                    block_start,
                    block_end
                );

                let index = self.load().await?;

                // copy as much as possible, up to the block boundary
                let buffer_offset = (self.current_offset - block_start) as usize;
                let bytes_to_read = buf.len();

                let end = core::cmp::min(buffer_offset + bytes_to_read, SIZE);
                trace!("buffer_offset {}, end {}", buffer_offset, end);
                let bytes_read = end - buffer_offset;
                buf[..bytes_read]
                    .copy_from_slice(&self.slots.as_mut()[index].buffer[buffer_offset..end]);
                buf = &mut buf[bytes_read..]; // move the buffer along

                bytes_read
            };

            self.current_offset += bytes_read as u64;
            total += bytes_read;

            if total == target {
                return Ok(total);
            }
        }
    }
}

impl<T: BlockDevice<SIZE>, const SIZE: usize, B: AsMut<[Slot<T::Align, SIZE>]>> Write
    for BufStream<T, SIZE, B>
{
    async fn write(&mut self, mut buf: &[u8]) -> Result<usize, Self::Error> {
        // A zero-length write must be a no-op: forwarding it would otherwise
        // satisfy the block-aligned fast path below and issue a 0-block device
        // transfer (which some block devices, e.g. STM32 SDMMC DMA, reject).
        if buf.is_empty() {
            return Ok(0);
        }
        let mut total = 0;
        let target = buf.len();
        loop {
            let bytes_written = if buf.len() % SIZE == 0
                && buf.as_ptr().cast::<u8>() as usize % Self::ALIGN == 0
                && self.current_offset % SIZE as u64 == 0
            {
                // If the provided buffer has a suitable length and alignment _and_ the write head is on a block boundary, use it directly
                // This write replaces the blocks in its range, so discard the ones in memory.
                for index in 0..self.slot_count() {
                    if self.in_range(index, buf.len()) {
                        let slot = &mut self.slots.as_mut()[index];
                        slot.block = NO_BLOCK;
                        slot.dirty = false;
                    }
                }
                let block = self.pointer_block_start();
                self.inner.write(block, slice_to_blocks(buf)).await?;

                buf.len()
            } else {
                let block_start = self.pointer_block_start_addr();
                let block_end = block_start + SIZE as u64;
                trace!(
                    "offset {}, block_start {}, block_end {}",
                    self.current_offset,
                    block_start,
                    block_end
                );

                // read the block into a slot if we need to
                let index = self.load().await?;

                // copy as much as possible, up to the block boundary
                let buffer_offset = (self.current_offset - block_start) as usize;
                let bytes_to_write = buf.len();

                let end = core::cmp::min(buffer_offset + bytes_to_write, SIZE);
                trace!("buffer_offset {}, end {}", buffer_offset, end);
                let bytes_written = end - buffer_offset;
                let slot = &mut self.slots.as_mut()[index];
                slot.buffer[buffer_offset..buffer_offset + bytes_written]
                    .copy_from_slice(&buf[..bytes_written]);
                buf = &buf[bytes_written..]; // move the buffer along

                // If we haven't written directly, the data is in the slot, which may need a flush later
                // so we mark it as dirty
                slot.dirty = true;

                // write out the whole block with the modified data
                if block_start + end as u64 == block_end {
                    trace!("Flushing sector cache");
                    self.flush_slot(index).await?;
                }

                bytes_written
            };

            self.current_offset += bytes_written as u64;
            total += bytes_written;

            if total == target {
                return Ok(total);
            }
        }
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.flush().await?;
        Ok(())
    }
}

impl<T: BlockDevice<SIZE>, const SIZE: usize, B: AsMut<[Slot<T::Align, SIZE>]>> Seek
    for BufStream<T, SIZE, B>
{
    async fn seek(&mut self, pos: SeekFrom) -> Result<u64, Self::Error> {
        let (base, offset) = match pos {
            SeekFrom::Start(x) => (x, 0),
            SeekFrom::End(x) => (self.inner.size().await?, x),
            SeekFrom::Current(x) => (self.current_offset, x),
        };
        // i128 holds every u64 + i64 sum, so this cannot overflow.
        self.current_offset = u64::try_from(i128::from(base) + i128::from(offset))
            .map_err(|_| BufStreamError::InvalidSeek)?;
        Ok(self.current_offset)
    }
}

#[cfg(test)]
mod tests {
    use aligned::A4;
    use embedded_io_async::ErrorType;

    use super::*;

    struct TestBlockDevice<T: Read + Write + Seek>(T);

    impl<T: Read + Write + Seek> ErrorType for TestBlockDevice<T> {
        type Error = T::Error;
    }

    impl<T: Read + Write + Seek> Read for TestBlockDevice<T> {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            Ok(self.0.read(buf).await?)
        }
    }

    impl<T: Read + Write + Seek> Write for TestBlockDevice<T> {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            Ok(self.0.write(buf).await?)
        }

        async fn flush(&mut self) -> Result<(), Self::Error> {
            self.0.flush().await
        }
    }

    impl<T: Read + Write + Seek> Seek for TestBlockDevice<T> {
        async fn seek(&mut self, pos: SeekFrom) -> Result<u64, Self::Error> {
            Ok(self.0.seek(pos).await?)
        }
    }

    impl<T: Read + Write + Seek> BlockDevice<512> for TestBlockDevice<T> {
        type Error = T::Error;
        type Align = aligned::A4;

        /// Read one or more blocks at the given block address.
        async fn read(
            &mut self,
            block_address: u32,
            data: &mut [Aligned<Self::Align, [u8; 512]>],
        ) -> Result<(), Self::Error> {
            self.0
                .seek(SeekFrom::Start((block_address * 512).into()))
                .await?;
            for b in data {
                self.0.read(&mut b[..]).await?;
            }
            Ok(())
        }

        /// Write one or more blocks at the given block address.
        async fn write(
            &mut self,
            block_address: u32,
            data: &[Aligned<Self::Align, [u8; 512]>],
        ) -> Result<(), Self::Error> {
            self.0
                .seek(SeekFrom::Start((block_address * 512).into()))
                .await?;
            for b in data {
                self.0.write(&b[..]).await?;
            }
            Ok(())
        }

        async fn size(&mut self) -> Result<u64, Self::Error> {
            Ok(u64::MAX)
        }
    }

    #[tokio::test]
    async fn block_512_read_test() {
        let _ = env_logger::builder().is_test(true).try_init();
        let buf = ("A".repeat(512) + "B".repeat(512).as_str()).into_bytes();
        let cur = std::io::Cursor::new(buf);
        let mut block: BufStream<_, 512> = BufStream::new(TestBlockDevice(
            embedded_io_adapters::tokio_1::FromTokio::new(cur),
        ));

        // Test sector aligned access
        let mut buf = vec![0; 128];
        block.seek(SeekFrom::Start(0)).await.unwrap();
        block.read_exact(&mut buf[..]).await.unwrap();
        assert_eq!(buf, "A".repeat(128).into_bytes());

        let mut buf = vec![0; 128];
        block.seek(SeekFrom::Start(512)).await.unwrap();
        block.read_exact(&mut buf[..]).await.unwrap();
        assert_eq!(buf, "B".repeat(128).into_bytes());

        // Read across sectors
        let mut buf = vec![0; 128];
        block.seek(SeekFrom::Start(512 - 64)).await.unwrap();
        block.read_exact(&mut buf[..]).await.unwrap();
        assert_eq!(buf, ("A".repeat(64) + "B".repeat(64).as_str()).into_bytes());
    }

    #[tokio::test]
    async fn block_512_read_successive() {
        let _ = env_logger::builder().is_test(true).try_init();
        let buf = ("A".repeat(64) + "B".repeat(64).as_str())
            .repeat(16)
            .into_bytes();
        let cur = std::io::Cursor::new(buf);
        let mut block: BufStream<_, 512> = BufStream::new(TestBlockDevice(
            embedded_io_adapters::tokio_1::FromTokio::new(cur),
        ));

        // Test sector aligned access
        let mut buf = vec![0; 64];
        block.seek(SeekFrom::Start(0)).await.unwrap();
        block.read_exact(&mut buf[..]).await.unwrap();
        assert_eq!(buf, "A".repeat(64).into_bytes());

        let mut buf = vec![0; 64];
        block.seek(SeekFrom::Start(64)).await.unwrap();
        block.read_exact(&mut buf[..]).await.unwrap();
        assert_eq!(buf, "B".repeat(64).into_bytes());

        let mut buf = vec![0; 64];
        block.seek(SeekFrom::Start(32)).await.unwrap();
        block.read_exact(&mut buf[..]).await.unwrap();
        assert_eq!(buf, ("A".repeat(32) + "B".repeat(32).as_str()).into_bytes());
    }

    #[tokio::test]
    async fn block_512_write_single_sector() {
        let _ = env_logger::builder().is_test(true).try_init();
        let buf = vec![0; 2048];
        let cur = std::io::Cursor::new(buf);
        let mut block: BufStream<_, 512> = BufStream::new(TestBlockDevice(
            embedded_io_adapters::tokio_1::FromTokio::new(cur),
        ));

        // Test sector aligned access
        let data_a = "A".repeat(512).into_bytes();
        block.seek(SeekFrom::Start(0)).await.unwrap();
        block.write_all(&data_a).await.unwrap();
        assert_eq!(
            &block.into_inner().0.into_inner().into_inner()[..512],
            data_a
        )
    }

    #[tokio::test]
    async fn block_512_write_across_sectors() {
        let _ = env_logger::builder().is_test(true).try_init();
        let buf = vec![0; 2048];
        let cur = std::io::Cursor::new(buf);
        let mut block: BufStream<_, 512> = BufStream::new(TestBlockDevice(
            embedded_io_adapters::tokio_1::FromTokio::new(cur),
        ));

        // Test sector aligned access
        let data_a = "A".repeat(512).into_bytes();
        block.seek(SeekFrom::Start(256)).await.unwrap();
        block.write_all(&data_a).await.unwrap();
        block.flush().await.unwrap();
        let buf = block.into_inner().0.into_inner().into_inner();
        assert_eq!(&buf[..256], [0; 256]);
        assert_eq!(&buf[256..768], data_a);
        assert_eq!(&buf[768..1024], [0; 256]);
    }

    #[tokio::test]
    async fn aligned_write_block_optimization() {
        let _ = env_logger::builder().is_test(true).try_init();
        let buf = vec![0; 2048];
        let cur = std::io::Cursor::new(buf);
        let mut block: BufStream<_, 512> = BufStream::new(TestBlockDevice(
            embedded_io_adapters::tokio_1::FromTokio::new(cur),
        ));

        let mut aligned_buffer: Aligned<A4, [u8; 512]> = Aligned([0; 512]);
        let data_a = "A".repeat(512).into_bytes();
        aligned_buffer[..].copy_from_slice(&data_a[..]);
        block.seek(SeekFrom::Start(0)).await.unwrap();
        block.write_all(&aligned_buffer[..]).await.unwrap();

        // if we wrote directly, the block buffer will be empty
        assert_eq!(&block.slots[0].buffer[..], [0u8; 512]);
        // ensure that the current offset is still updated
        assert_eq!(block.current_offset, 512);
        // the write suceeded
        assert_eq!(
            &block.into_inner().0.into_inner().into_inner()[..512],
            &data_a
        )
    }

    #[tokio::test]
    async fn aligned_write_block_optimization_misaligned_block() {
        let _ = env_logger::builder().is_test(true).try_init();
        let buf = vec![0; 2048];
        let cur = std::io::Cursor::new(buf);
        let mut block: BufStream<_, 512> = BufStream::new(TestBlockDevice(
            embedded_io_adapters::tokio_1::FromTokio::new(cur),
        ));

        let mut aligned_buffer: Aligned<A4, [u8; 2048]> = Aligned([0; 2048]);
        let data_a = "A".repeat(512).into_bytes();
        aligned_buffer[..512].copy_from_slice(&data_a[..]);
        // seek away from aligned block address
        block.seek(SeekFrom::Start(3)).await.unwrap();
        // attempt write all
        block.write_all(&aligned_buffer[..512]).await.unwrap();
        block.flush().await.unwrap();

        // because the addr was not block aligned, we will have used the cache
        assert_ne!(&block.slots[0].buffer[..], [0u8; 512]);
        // the write suceeded
        assert_eq!(
            &block.into_inner().0.into_inner().into_inner()[3..515],
            &data_a
        )
    }

    #[tokio::test]
    async fn aligned_read_block_optimization() {
        let _ = env_logger::builder().is_test(true).try_init();
        let buf = "A".repeat(2048).into_bytes();
        let cur = std::io::Cursor::new(buf);
        let mut block: BufStream<_, 512> = BufStream::new(TestBlockDevice(
            embedded_io_adapters::tokio_1::FromTokio::new(cur),
        ));

        let mut aligned_buffer: Aligned<A4, [u8; 512]> = Aligned([0; 512]);
        block.seek(SeekFrom::Start(0)).await.unwrap();
        block.read_exact(&mut aligned_buffer[..]).await.unwrap();

        // if we read directly, the block buffer will be empty
        assert_eq!(&block.slots[0].buffer[..], [0u8; 512]);
        // ensure that the current offset is still updated
        assert_eq!(block.current_offset, 512);
        // the write suceeded
        assert_eq!(
            &block.into_inner().0.into_inner().into_inner()[..512],
            &aligned_buffer[..]
        )
    }

    #[tokio::test]
    async fn aligned_read_block_optimization_misaligned() {
        let _ = env_logger::builder().is_test(true).try_init();
        let buf = "A".repeat(2048).into_bytes();
        let cur = std::io::Cursor::new(buf);
        let mut block: BufStream<_, 512> = BufStream::new(TestBlockDevice(
            embedded_io_adapters::tokio_1::FromTokio::new(cur),
        ));

        let mut aligned_buffer: Aligned<A4, [u8; 512]> = Aligned([0; 512]);
        // seek away from aligned block
        block.seek(SeekFrom::Start(3)).await.unwrap();
        // pass an aligned buffer with correct sizing
        block.read_exact(&mut aligned_buffer[..]).await.unwrap();

        // now, we must seek back and read the entire block
        // meaning our block cache will be written to:
        assert_ne!(&block.slots[0].buffer[..], [0u8; 512]);

        // the read suceeded
        assert_eq!(
            &block.into_inner().0.into_inner().into_inner()[3..512],
            &aligned_buffer[3..]
        )
    }

    #[tokio::test]
    async fn write_seek_read_write() {
        let _ = env_logger::builder().is_test(true).try_init();
        let buf = "A".repeat(2048).into_bytes();
        let cur = std::io::Cursor::new(buf);
        let mut block: BufStream<_, 512> = BufStream::new(TestBlockDevice(
            embedded_io_adapters::tokio_1::FromTokio::new(cur),
        ));

        block.seek(SeekFrom::Start(524)).await.unwrap();
        block
            .write_all(&"B".repeat(512).into_bytes())
            .await
            .unwrap();
        block.flush().await.unwrap();

        block.seek(SeekFrom::Start(0)).await.unwrap();
        let mut tmp = [0u8; 256];
        block.read(&mut tmp[..]).await.unwrap();

        assert_eq!(&tmp[..], "A".repeat(256).into_bytes().as_slice());

        block.seek(SeekFrom::Start(524 + 512)).await.unwrap();
        block
            .write_all(&"C".repeat(512).into_bytes())
            .await
            .unwrap();
        block.flush().await.unwrap();

        let buf = block.into_inner().0.into_inner().into_inner();

        assert_eq!(
            buf,
            ("A".repeat(524) + &"B".repeat(512) + &"C".repeat(512) + &"A".repeat(500)).into_bytes()
        )
    }

    /// A block device that panics if ever handed a 0-block transfer, used to
    /// prove the `BufStream` empty-buffer guard never forwards one.
    struct AssertNonEmptyBlockDevice;

    impl BlockDevice<512> for AssertNonEmptyBlockDevice {
        type Error = core::convert::Infallible;
        type Align = A4;

        async fn read(
            &mut self,
            _block_address: u32,
            data: &mut [Aligned<A4, [u8; 512]>],
        ) -> Result<(), Self::Error> {
            assert!(!data.is_empty(), "0-block read forwarded to device");
            Ok(())
        }

        async fn write(
            &mut self,
            _block_address: u32,
            data: &[Aligned<A4, [u8; 512]>],
        ) -> Result<(), Self::Error> {
            assert!(!data.is_empty(), "0-block write forwarded to device");
            Ok(())
        }

        async fn size(&mut self) -> Result<u64, Self::Error> {
            Ok(u64::MAX)
        }
    }

    #[tokio::test]
    async fn empty_transfers_are_noops() {
        let mut block: BufStream<_, 512> = BufStream::new(AssertNonEmptyBlockDevice);
        block.seek(SeekFrom::Start(0)).await.unwrap();

        // Empty read/write must return Ok(0) without touching the device.
        assert_eq!(block.write(&[]).await.unwrap(), 0);
        assert_eq!(block.read(&mut []).await.unwrap(), 0);

        // A real aligned transfer still reaches the device (sanity check that
        // the assert above is actually armed).
        let aligned: Aligned<A4, [u8; 512]> = Aligned([0xAB; 512]);
        block.write_all(&aligned[..]).await.unwrap();
    }

    #[tokio::test]
    async fn seek_from_end_and_current() {
        let cur = std::io::Cursor::new(vec![0; 2048]);
        let mut block: BufStream<_, 512> = BufStream::new(TestBlockDevice(
            embedded_io_adapters::tokio_1::FromTokio::new(cur),
        ));
        // TestBlockDevice reports a size of u64::MAX.
        assert_eq!(block.seek(SeekFrom::End(-10)).await.unwrap(), u64::MAX - 10);
        assert_eq!(block.seek(SeekFrom::End(0)).await.unwrap(), u64::MAX);
        assert_eq!(
            block.seek(SeekFrom::Current(-5)).await.unwrap(),
            u64::MAX - 5
        );

        block.seek(SeekFrom::Start(100)).await.unwrap();
        assert_eq!(block.seek(SeekFrom::Current(-50)).await.unwrap(), 50);
        assert_eq!(block.seek(SeekFrom::Current(25)).await.unwrap(), 75);
    }

    #[tokio::test]
    async fn invalid_seek_is_error() {
        let mut block: BufStream<_, 512> = BufStream::new(AssertNonEmptyBlockDevice);
        block.seek(SeekFrom::Start(100)).await.unwrap();

        assert_eq!(
            block.seek(SeekFrom::Current(-101)).await,
            Err(BufStreamError::InvalidSeek)
        );
        assert_eq!(
            block.seek(SeekFrom::End(1)).await,
            Err(BufStreamError::InvalidSeek)
        );
        // A failed seek does not move the position.
        assert_eq!(block.seek(SeekFrom::Current(0)).await.unwrap(), 100);

        block.seek(SeekFrom::Start(u64::MAX)).await.unwrap();
        assert_eq!(
            block.seek(SeekFrom::Current(1)).await,
            Err(BufStreamError::InvalidSeek)
        );
    }

    fn zeroed_block_stream() -> BufStream<
        TestBlockDevice<embedded_io_adapters::tokio_1::FromTokio<std::io::Cursor<Vec<u8>>>>,
        512,
    > {
        let cur = std::io::Cursor::new(vec![0; 2048]);
        BufStream::new(TestBlockDevice(
            embedded_io_adapters::tokio_1::FromTokio::new(cur),
        ))
    }

    #[tokio::test]
    async fn aligned_read_sees_dirty_cache() {
        let mut block = zeroed_block_stream();
        block.write_all(b"hello").await.unwrap();

        let mut aligned: Aligned<A4, [u8; 1024]> = Aligned([0; 1024]);
        block.seek(SeekFrom::Start(0)).await.unwrap();
        block.read_exact(&mut aligned[..]).await.unwrap();
        assert_eq!(&aligned[..5], b"hello");
    }

    #[tokio::test]
    async fn aligned_write_is_not_overwritten_by_dirty_cache() {
        let mut block = zeroed_block_stream();
        block.write_all(b"old").await.unwrap();

        let aligned: Aligned<A4, [u8; 512]> = Aligned([0xAB; 512]);
        block.seek(SeekFrom::Start(0)).await.unwrap();
        block.write_all(&aligned[..]).await.unwrap();
        block.flush().await.unwrap();

        let buf = block.into_inner().0.into_inner().into_inner();
        assert_eq!(&buf[..512], &aligned[..]);
    }

    #[tokio::test]
    async fn read_after_aligned_write_is_not_stale() {
        let mut block = zeroed_block_stream();
        let mut tmp = [0u8; 4];
        block.read_exact(&mut tmp).await.unwrap();

        let aligned: Aligned<A4, [u8; 512]> = Aligned([0xAB; 512]);
        block.seek(SeekFrom::Start(0)).await.unwrap();
        block.write_all(&aligned[..]).await.unwrap();

        block.seek(SeekFrom::Start(0)).await.unwrap();
        block.read_exact(&mut tmp).await.unwrap();
        assert_eq!(tmp, [0xAB; 4]);
    }

    #[tokio::test]
    async fn aligned_transfer_outside_cache_keeps_cache() {
        let mut block = zeroed_block_stream();
        block.write_all(b"cached").await.unwrap();

        // Blocks 1..3 do not contain the cached block 0.
        let aligned: Aligned<A4, [u8; 1024]> = Aligned([0xAB; 1024]);
        block.seek(SeekFrom::Start(512)).await.unwrap();
        block.write_all(&aligned[..]).await.unwrap();
        block.flush().await.unwrap();

        let buf = block.into_inner().0.into_inner().into_inner();
        assert_eq!(&buf[..6], b"cached");
        assert_eq!(&buf[512..1536], &aligned[..]);
    }

    /// In-memory block device whose next read or write can be made to fail once. A failing read
    /// first clobbers the destination, as a driver does when a CRC check fails after the transfer.
    struct FlakyDevice {
        data: Vec<u8>,
        fail_next_read: bool,
        fail_next_write: bool,
    }

    #[derive(Debug)]
    struct Failed;

    impl BlockDevice<512> for FlakyDevice {
        type Error = Failed;
        type Align = aligned::A4;

        async fn read(
            &mut self,
            block_address: u32,
            data: &mut [Aligned<Self::Align, [u8; 512]>],
        ) -> Result<(), Self::Error> {
            if core::mem::take(&mut self.fail_next_read) {
                for block in data.iter_mut() {
                    block.fill(0xEE);
                }
                return Err(Failed);
            }
            for (index, block) in data.iter_mut().enumerate() {
                let start = (block_address as usize + index) * 512;
                block.copy_from_slice(&self.data[start..start + 512]);
            }
            Ok(())
        }

        async fn write(
            &mut self,
            block_address: u32,
            data: &[Aligned<Self::Align, [u8; 512]>],
        ) -> Result<(), Self::Error> {
            if core::mem::take(&mut self.fail_next_write) {
                return Err(Failed);
            }
            for (index, block) in data.iter().enumerate() {
                let start = (block_address as usize + index) * 512;
                self.data[start..start + 512].copy_from_slice(&block[..]);
            }
            Ok(())
        }

        async fn size(&mut self) -> Result<u64, Self::Error> {
            Ok(self.data.len() as u64)
        }
    }

    fn flaky_stream() -> BufStream<FlakyDevice, 512> {
        let mut data = vec![0; 2048];
        data[..4].copy_from_slice(b"blk0");
        data[512..516].copy_from_slice(b"blk1");
        BufStream::new(FlakyDevice {
            data,
            fail_next_read: false,
            fail_next_write: false,
        })
    }

    #[tokio::test]
    async fn failed_flush_keeps_data_for_retry() {
        let mut block = flaky_stream();
        block.write_all(b"new!").await.unwrap();
        block.inner.fail_next_write = true;
        assert!(block.flush().await.is_err());

        block.flush().await.unwrap();
        assert_eq!(&block.into_inner().data[..4], b"new!");
    }

    #[tokio::test]
    async fn failed_read_does_not_leave_garbage_under_old_block() {
        let mut block = flaky_stream();
        let mut tmp = [0u8; 4];
        block.read_exact(&mut tmp).await.unwrap();
        assert_eq!(&tmp, b"blk0");

        block.inner.fail_next_read = true;
        block.seek(SeekFrom::Start(512)).await.unwrap();
        assert!(block.read_exact(&mut tmp).await.is_err());

        block.seek(SeekFrom::Start(0)).await.unwrap();
        block.read_exact(&mut tmp).await.unwrap();
        assert_eq!(&tmp, b"blk0");
    }

    #[tokio::test]
    async fn partial_write_after_failed_read_keeps_old_block() {
        let mut block = flaky_stream();
        let mut tmp = [0u8; 4];
        block.read_exact(&mut tmp).await.unwrap();

        block.inner.fail_next_read = true;
        block.seek(SeekFrom::Start(512)).await.unwrap();
        assert!(block.read_exact(&mut tmp).await.is_err());

        block.seek(SeekFrom::Start(4)).await.unwrap();
        block.write_all(b"ab").await.unwrap();
        block.flush().await.unwrap();

        let data = block.into_inner().data;
        assert_eq!(&data[..6], b"blk0ab");
        assert!(data[6..512].iter().all(|&byte| byte == 0));
        assert_eq!(&data[512..516], b"blk1");
    }

    /// Device that counts its block reads and writes.
    struct CountingDevice {
        data: Vec<u8>,
        reads: usize,
        writes: usize,
        fail_next_write: bool,
    }

    impl BlockDevice<512> for CountingDevice {
        type Error = Failed;
        type Align = aligned::A4;

        async fn read(
            &mut self,
            block_address: u32,
            data: &mut [Aligned<Self::Align, [u8; 512]>],
        ) -> Result<(), Self::Error> {
            self.reads += 1;
            for (index, block) in data.iter_mut().enumerate() {
                let start = (block_address as usize + index) * 512;
                block.copy_from_slice(&self.data[start..start + 512]);
            }
            Ok(())
        }

        async fn write(
            &mut self,
            block_address: u32,
            data: &[Aligned<Self::Align, [u8; 512]>],
        ) -> Result<(), Self::Error> {
            if core::mem::take(&mut self.fail_next_write) {
                return Err(Failed);
            }
            self.writes += 1;
            for (index, block) in data.iter().enumerate() {
                let start = (block_address as usize + index) * 512;
                self.data[start..start + 512].copy_from_slice(&block[..]);
            }
            Ok(())
        }

        async fn size(&mut self) -> Result<u64, Self::Error> {
            Ok(self.data.len() as u64)
        }
    }

    type CountingStream<const SLOTS: usize> =
        BufStream<CountingDevice, 512, [Slot<A4, 512>; SLOTS]>;

    fn counting_stream<const SLOTS: usize>() -> CountingStream<SLOTS> {
        BufStream::with_slots(
            CountingDevice {
                data: vec![0; 4096],
                reads: 0,
                writes: 0,
                fail_next_write: false,
            },
            core::array::from_fn(|_| Slot::new()),
        )
    }

    /// Writes four bytes at the same offset of two blocks in turn, as a filesystem does for an entry of a
    /// mirrored table.
    async fn alternate<const SLOTS: usize>(block: &mut CountingStream<SLOTS>, rounds: usize) {
        for round in 0..rounds {
            for start in [0, 1024] {
                block
                    .seek(SeekFrom::Start(start + 4 * round as u64))
                    .await
                    .unwrap();
                block.write_all(&[round as u8 + 1; 4]).await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn two_slots_keep_two_alternating_blocks_in_memory() {
        let mut block = counting_stream::<2>();
        alternate(&mut block, 10).await;
        assert_eq!((block.inner.reads, block.inner.writes), (2, 0));
        block.flush().await.unwrap();
        let device = block.into_inner();
        assert_eq!((device.reads, device.writes), (2, 2));
        for round in 0..10 {
            for start in [0, 1024] {
                let at = start + 4 * round;
                assert_eq!(&device.data[at..at + 4], [round as u8 + 1; 4]);
            }
        }
    }

    #[tokio::test]
    async fn a_third_block_takes_the_slot_used_longest_ago() {
        let mut block = counting_stream::<2>();
        block.write_all(b"zero").await.unwrap();
        block.seek(SeekFrom::Start(512)).await.unwrap();
        block.write_all(b"one!").await.unwrap();
        // Block 0 is used again, so block 1 is the one to give way.
        block.seek(SeekFrom::Start(4)).await.unwrap();
        block.write_all(b"more").await.unwrap();
        block.seek(SeekFrom::Start(1024)).await.unwrap();
        block.write_all(b"two!").await.unwrap();
        // Block 1 reached the device when it lost its slot; blocks 0 and 2 are still only in memory.
        assert_eq!(block.inner.writes, 1);
        assert_eq!(&block.inner.data[512..516], b"one!");
        assert_eq!(&block.inner.data[..4], [0; 4]);

        let mut tmp = [0u8; 8];
        block.seek(SeekFrom::Start(0)).await.unwrap();
        block.read_exact(&mut tmp).await.unwrap();
        assert_eq!(&tmp, b"zeromore");
        assert_eq!(block.inner.reads, 3, "block 0 was still in memory");

        block.flush().await.unwrap();
        let device = block.into_inner();
        assert_eq!(&device.data[..8], b"zeromore");
        assert_eq!(&device.data[1024..1028], b"two!");
    }

    #[tokio::test]
    async fn flush_writes_every_modified_block_and_retries_after_a_failure() {
        let mut block = counting_stream::<2>();
        alternate(&mut block, 1).await;
        block.inner.fail_next_write = true;
        assert!(block.flush().await.is_err());
        assert_eq!(block.inner.writes, 0);

        block.flush().await.unwrap();
        assert_eq!(block.inner.writes, 2);
        // Nothing is left to write.
        block.flush().await.unwrap();
        let device = block.into_inner();
        assert_eq!(device.writes, 2);
        assert_eq!(&device.data[..4], [1; 4]);
        assert_eq!(&device.data[1024..1028], [1; 4]);
    }

    #[tokio::test]
    async fn aligned_transfers_see_and_replace_blocks_in_any_slot() {
        let mut block = counting_stream::<2>();
        alternate(&mut block, 1).await;

        // An aligned read of blocks 0..4 gets the modified data of both slots.
        let mut whole: Aligned<A4, [u8; 2048]> = Aligned([0; 2048]);
        block.seek(SeekFrom::Start(0)).await.unwrap();
        block.read_exact(&mut whole[..]).await.unwrap();
        assert_eq!(&whole[..4], [1; 4]);
        assert_eq!(&whole[1024..1028], [1; 4]);

        // An aligned write over both blocks replaces them; the slots must not write old data back.
        alternate(&mut block, 1).await;
        let fresh: Aligned<A4, [u8; 2048]> = Aligned([9; 2048]);
        block.seek(SeekFrom::Start(0)).await.unwrap();
        block.write_all(&fresh[..]).await.unwrap();
        block.flush().await.unwrap();

        let mut tmp = [0u8; 4];
        block.seek(SeekFrom::Start(1024)).await.unwrap();
        block.read_exact(&mut tmp).await.unwrap();
        assert_eq!(tmp, [9; 4]);
        assert!(block.into_inner().data[..2048]
            .iter()
            .all(|&byte| byte == 9));
    }

    #[tokio::test]
    async fn slots_kept_outside_the_stream_start_empty() {
        let mut slots: [Slot<A4, 512>; 2] = [Slot::new(), Slot::new()];
        let device = {
            let mut block = BufStream::with_slots(
                CountingDevice {
                    data: vec![0; 4096],
                    reads: 0,
                    writes: 0,
                    fail_next_write: false,
                },
                &mut slots,
            );
            // Left modified in a slot and never flushed, as when a card is pulled.
            block.write_all(b"lost").await.unwrap();
            block.into_inner()
        };
        assert_eq!(&device.data[..4], [0; 4]);

        // A second stream on the same slots reads the device, and writes nothing of the first one's.
        let mut block = BufStream::with_slots(device, &mut slots);
        let mut tmp = [0xFFu8; 4];
        block.read_exact(&mut tmp).await.unwrap();
        assert_eq!(tmp, [0; 4]);
        block.flush().await.unwrap();
        let device = block.into_inner();
        // One read by each stream; the unflushed data of the first was never written.
        assert_eq!((device.reads, device.writes), (2, 0));
    }
}
