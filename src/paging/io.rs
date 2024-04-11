//! Block size based IO reading and writing.

use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

use super::pager::PageNumber;

/// Some common operations that we need to execute on files and are not provided
/// by traits in [`std::io`]
pub(crate) trait FileOps {
    /// Creates a file on the filesystem at the given `path`.
    ///
    /// If the file already exists it should be truncated and if the parent
    /// directories are not present they will be creates as well.
    fn create(path: impl AsRef<Path>) -> io::Result<Self>
    where
        Self: Sized;

    /// Opens the file "as is", no trunc.
    fn open(path: impl AsRef<Path>) -> io::Result<Self>
    where
        Self: Sized;

    /// Removes the file located at `path`.
    fn remove(path: impl AsRef<Path>) -> io::Result<()>;

    /// Truncates the file to 0 length.
    fn truncate(&mut self) -> io::Result<()>;

    /// Attempts to persist the data to its destination.
    ///
    /// For disk filesystems this should use the necessary syscalls to send
    /// everything to the hardware. On Unix systems there are two main ways to
    /// achieve this: `fflush()` and `fsync()`. Flushing is already implemented
    /// for us in [`Write`] but it's not enough to write contents to disk. See
    /// this [StackOverflow question] for details:
    ///
    /// [StackOverflow question]: https://stackoverflow.com/questions/2340610/difference-between-fflush-and-fsync
    fn sync(&self) -> io::Result<()>;
}

impl FileOps for File {
    fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)?
        }

        File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)
    }

    fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        File::options().read(true).write(false).open(path)
    }

    fn remove(path: impl AsRef<Path>) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn truncate(&mut self) -> io::Result<()> {
        self.set_len(0)
    }

    // Luckily this time we don't have to dive into libc and start doing FFI.
    fn sync(&self) -> io::Result<()> {
        self.sync_all()
    }
}

/// In-memory buffer with the same trait implementations as a normal disk file.
///
/// Used mainly for tests, although we could use this to simulate an in-memory
/// database.
pub(crate) type MemBuf = io::Cursor<Vec<u8>>;

impl FileOps for MemBuf {
    fn create(_path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(io::Cursor::new(Vec::new()))
    }

    // TODO: HashMap of Path -> Cursor.
    // That would allow us to simulate a file system for tests.
    fn open(_path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(io::Cursor::new(Vec::new()))
    }

    fn truncate(&mut self) -> io::Result<()> {
        self.set_position(0);
        self.get_mut().clear();

        Ok(())
    }

    fn remove(_path: impl AsRef<Path>) -> io::Result<()> {
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        Ok(())
    }
}

/// Implements reading and writing based on the given block and page sizes.
///
/// This is how a file of block size 512 and page size 1024 would look like:
///
/// ```text
/// OFFSET      BLOCKS
///         +-------------+
///       0 | +---------+ |
///         | | BLOCK 0 | |
///         | +---------+ | PAGE 0
///     512 | +---------+ |
///         | | BLOCK 1 | |
///         | +---------+ |
///         +-------------+
///
///         +-------------+
///    1024 | +---------+ |
///         | | BLOCK 2 | |
///         | +---------+ | PAGE 1
///    1536 | +---------+ |
///         | | BLOCK 3 | |
///         | +---------+ |
///         +-------------+
/// ```
///
/// This struct is similar to a buffered reader or writer in the sense that it
/// wraps an IO handle and operates on top of it, but instead of buffering
/// reads and writes it returns full pages abstracting the blocks.
///
/// See [`BlockIo::read`] for more details on how it works.
#[derive(Debug, PartialEq)]
pub(super) struct BlockIo<I> {
    /// Underlying IO resource handle.
    io: I,
    /// Hardware / file system block size or prefered IO read/write buffer size.
    pub block_size: usize,
    /// High level page size.
    pub page_size: usize,
}

impl<I> BlockIo<I> {
    pub fn new(io: I, page_size: usize, block_size: usize) -> Self {
        Self {
            io,
            block_size,
            page_size,
        }
    }

    /// Some sanity checks for development.
    fn debug_assert_args_are_correct(&self, page_number: PageNumber, buf: &[u8]) {
        // We should always read and write an entire page.
        debug_assert!(
            buf.len() == self.page_size,
            "buffer of incorrect length {} given for page size {}",
            buf.len(),
            self.page_size
        );

        // Used for development/debugging in case we mess up. Don't wanna create
        // giant Gigabyte sized files all of a sudden. The maximum limit is
        // 100 MiB.
        debug_assert!(
            self.page_size * (page_number as usize) < (100 << 20),
            "page number {page_number} too high for page_size {}: limit is 100 MiB",
            self.page_size,
        );
    }
}

impl<I: Seek + Read> BlockIo<I> {
    /// Reads the raw bytes of the disk page in memory without doing anything
    /// else.
    ///
    /// Simplest case is when [`Self::page_size`] >= [`Self::block_size`]. For
    /// example, suppose `block_size = 512` and `page_size = 1024`:
    ///
    /// ```text
    /// OFFSET      BLOCKS
    ///         +-------------+
    ///       0 | +---------+ |
    ///         | | BLOCK 0 | |
    ///         | +---------+ | PAGE 0
    ///     512 | +---------+ |
    ///         | | BLOCK 1 | |
    ///         | +---------+ |
    ///         +-------------+
    ///
    ///         +-------------+
    ///    1024 | +---------+ |
    ///         | | BLOCK 2 | |
    ///         | +---------+ | PAGE 1
    ///    1536 | +---------+ |
    ///         | | BLOCK 3 | |
    ///         | +---------+ |
    ///         +-------------+
    /// ```
    ///
    /// Finding a page is as simple as computing `page_number * page_size`. The
    /// second, less trivial case, is when `block_size > page_size`, because
    /// many pages can fit into one block. Suppose `block_size = 4096` and
    /// `page_size = 1024`.
    ///
    /// ```text
    /// BLOCK     PAGE
    /// OFFSET    OFFSET    BLOCKS
    ///
    ///   0 ------->   +-------------+
    ///              0 | +---------+ |
    ///                | | PAGE  0 | |
    ///                | +---------+ |
    ///           1024 | +---------+ |
    ///                | | PAGE  1 | |
    ///                | +---------+ | BLOCK 0
    ///           2048 | +---------+ |
    ///                | | PAGE  2 | |
    ///                | +---------+ |
    ///           3072 | +---------+ |
    ///                | | PAGE  3 | |
    ///                | +---------+ |
    ///                +-------------+
    ///
    /// 4096 ------>   +-------------+
    ///           4096 | +---------+ |
    ///                | | PAGE  4 | |
    ///                | +---------+ |
    ///           5120 | +---------+ |
    ///                | | PAGE  5 | |
    ///                | +---------+ | BLOCK 0
    ///           6144 | +---------+ |
    ///                | | PAGE  6 | |
    ///                | +---------+ |
    ///           7168 | +---------+ |
    ///                | | PAGE  7 | |
    ///                | +---------+ |
    ///                +-------------+
    /// ```
    ///
    /// In this case we'll have to compute the page offset and align downwards
    /// to block size. For example, if we want page 6, first we compute
    /// `1024 * 6 = 6144` and then align `6144` downwards to `4096` to read the
    /// block in memory. Alignment on powers of two can be computed using XOR
    /// and a bitmask. Check [address alignment] for more details.
    ///
    /// [address alignment]: https://os.phil-opp.com/allocator-designs/#address-alignment
    pub fn read(&mut self, page_number: PageNumber, buf: &mut [u8]) -> io::Result<usize> {
        self.debug_assert_args_are_correct(page_number, buf);

        // Compute block offset and inner page offset.
        let (capacity, block_offset, inner_offset) = {
            let page_number = page_number as usize;
            let Self {
                page_size,
                block_size,
                ..
            } = *self;

            if page_size >= block_size {
                (page_size, page_size * page_number, 0)
            } else {
                let offset = (page_number * page_size) & !(block_size - 1);
                (block_size, offset, page_number * page_size - offset)
            }
        };

        // Spin the disk... or let SSD transistors go brrr.
        self.io.seek(SeekFrom::Start(block_offset as u64))?;

        // Read page into memory.
        if self.page_size >= self.block_size {
            return self.io.read(buf);
        }

        // If the block size is greater than page size, we're reading multiple
        // pages in one call. TODO: Find a way to cache all the pages, not just
        // one.
        let mut block = vec![0; capacity];
        let _ = self.io.read(&mut block)?;
        buf.copy_from_slice(&block[inner_offset..inner_offset + self.page_size]);

        Ok(self.page_size)
    }
}

impl<I: Seek + Write> BlockIo<I> {
    /// Writes the page to disk. See also [`Self::read`] for more details.
    pub fn write(&mut self, page_number: PageNumber, buf: &[u8]) -> io::Result<usize> {
        self.debug_assert_args_are_correct(page_number, buf);

        // TODO: Just like [`Self::read`], when the block size is greater than
        // the page size we should be writing multiple pages at once.
        let offset = self.page_size * page_number as usize;
        self.io.seek(SeekFrom::Start(offset as u64))?;

        // TODO: If page_size > block_size check if all blocks need to be written
        self.io.write(buf)
    }
}

impl<I: Write> BlockIo<I> {
    /// Flush buffered contents.
    ///
    /// This does not guarantee that the contents reach the filesystem. Use
    /// [`Self::sync`] after flushing.
    pub fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

impl<I: FileOps> BlockIo<I> {
    /// See [`Sync`] for details.
    pub fn sync(&self) -> io::Result<()> {
        self.io.sync()
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::BlockIo;

    #[test]
    fn block_io() -> io::Result<()> {
        let sizes = [(4, 4), (4, 16), (16, 4)];

        for (page_size, block_size) in sizes {
            let max_pages = 10;

            let mem_buf = io::Cursor::new(Vec::new());

            let mut io = BlockIo::new(mem_buf, page_size, block_size);

            for i in 0..max_pages {
                let expected = vec![(i + 1) as u8; page_size];
                let mut buf = vec![0; page_size];

                assert_eq!(io.write(i, &expected)?, page_size);
                assert_eq!(io.read(i, &mut buf)?, buf.len());
                assert_eq!(buf, expected);
            }
        }

        Ok(())
    }
}
