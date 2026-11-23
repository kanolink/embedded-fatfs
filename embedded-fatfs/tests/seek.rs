//! A seek back into the cluster before the current one must not walk the cluster chain from the
//! first cluster. A log writer that rewrites a partly written block from its start, where the
//! block starts on a cluster boundary, does this after every flush.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use embedded_fatfs::{FatType, FileSystem, FormatVolumeOptions, FsOptions};
use embedded_io_async::{ErrorKind, ErrorType, Read, Seek, SeekFrom, Write};

/// In-memory volume that counts its reads.
struct Storage {
    data: Arc<Mutex<Vec<u8>>>,
    pos: u64,
    reads: Arc<AtomicUsize>,
}

impl ErrorType for Storage {
    type Error = ErrorKind;
}

impl Read for Storage {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let data = self.data.lock().unwrap();
        let start = self.pos as usize;
        let len = buf.len().min(data.len().saturating_sub(start));
        buf[..len].copy_from_slice(&data[start..start + len]);
        self.pos += len as u64;
        Ok(len)
    }
}

impl Write for Storage {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let mut data = self.data.lock().unwrap();
        let start = self.pos as usize;
        data[start..start + buf.len()].copy_from_slice(buf);
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl Seek for Storage {
    async fn seek(&mut self, pos: SeekFrom) -> Result<u64, Self::Error> {
        let len = self.data.lock().unwrap().len() as i64;
        self.pos = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => len + offset,
            SeekFrom::Current(offset) => self.pos as i64 + offset,
        } as u64;
        Ok(self.pos)
    }
}

type Fs = FileSystem<Storage, embedded_fatfs::DefaultTimeProvider, embedded_fatfs::LossyOemCpConverter>;

async fn volume() -> (Fs, Arc<AtomicUsize>) {
    let data = Arc::new(Mutex::new(vec![0u8; 40 * 1024 * 1024]));
    let reads = Arc::new(AtomicUsize::new(0));
    let mut storage = Storage {
        data,
        pos: 0,
        reads: reads.clone(),
    };
    embedded_fatfs::format_volume(&mut storage, FormatVolumeOptions::new().fat_type(FatType::Fat32))
        .await
        .unwrap();
    storage.pos = 0;
    let fs = FileSystem::new(storage, FsOptions::new()).await.unwrap();
    (fs, reads)
}

fn pattern(offset: usize, len: usize) -> Vec<u8> {
    (offset..offset + len).map(|i| (i % 251) as u8).collect()
}

const CLUSTERS: usize = 64;

#[tokio::test]
async fn rewriting_from_the_start_of_the_current_cluster_reads_nothing() {
    let (fs, reads) = volume().await;
    let cluster = fs.cluster_size() as usize;
    let mut file = fs.root_dir().create_file("RAW.LOG").await.unwrap();
    let mut len = CLUSTERS * cluster;
    file.write_all(&pattern(0, len)).await.unwrap();

    // Flushed tails: a partial block at the start of a cluster, written again with more data
    // each time, until it fills the cluster and the next block starts in the next one.
    for _ in 0..3 {
        let block = len / cluster * cluster;
        let tail = cluster / 4;
        file.seek(SeekFrom::Start(block as u64)).await.unwrap();
        file.write_all(&pattern(block, len - block + tail)).await.unwrap();
        len += tail;

        let before = reads.load(Ordering::Relaxed);
        assert_eq!(file.seek(SeekFrom::Start(block as u64)).await.unwrap(), block as u64);
        assert_eq!(reads.load(Ordering::Relaxed), before, "seek walked the cluster chain");
        file.write_all(&pattern(block, len - block + cluster / 2))
            .await
            .unwrap();
        len += cluster / 2;
    }
    // The rewritten block grows into the next cluster, which the write still finds.
    let block = len / cluster * cluster;
    file.seek(SeekFrom::Start(block as u64 - 8)).await.unwrap();
    file.write_all(&pattern(block - 8, len - block + 8 + cluster))
        .await
        .unwrap();
    len += cluster;
    file.flush().await.unwrap();

    file.seek(SeekFrom::Start(0)).await.unwrap();
    let mut back = vec![0u8; len];
    file.read_exact(&mut back).await.unwrap();
    assert_eq!(back, pattern(0, len));
    let mut more = [0u8; 1];
    assert_eq!(file.read(&mut more).await.unwrap(), 0);
}

#[tokio::test]
async fn seeking_further_back_still_finds_the_right_cluster() {
    let (fs, _) = volume().await;
    let cluster = fs.cluster_size() as usize;
    let mut file = fs.root_dir().create_file("RAW.LOG").await.unwrap();
    let len = CLUSTERS * cluster + cluster / 2;
    file.write_all(&pattern(0, len)).await.unwrap();

    // Back one cluster, which uses the cluster before the current one, and then back again,
    // which the file does not remember and walks for.
    for offset in [
        CLUSTERS * cluster,
        (CLUSTERS - 1) * cluster,
        (CLUSTERS - 2) * cluster + 3,
        (CLUSTERS - 3) * cluster,
    ] {
        file.seek(SeekFrom::Start(offset as u64)).await.unwrap();
        let mut back = vec![0u8; 16];
        file.read_exact(&mut back).await.unwrap();
        assert_eq!(back, pattern(offset, 16), "read at {offset}");
    }

    // A read across a boundary also remembers the cluster before it.
    file.seek(SeekFrom::Start((5 * cluster - 4) as u64)).await.unwrap();
    let mut back = vec![0u8; 8];
    file.read_exact(&mut back).await.unwrap();
    file.seek(SeekFrom::Start((5 * cluster - 2) as u64)).await.unwrap();
    file.read_exact(&mut back).await.unwrap();
    assert_eq!(back, pattern(5 * cluster - 2, 8));
}
