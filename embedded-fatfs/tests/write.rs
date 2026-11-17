use std::future::Future;
use std::str;
use tokio::fs;

use embedded_fatfs::{ChronoTimeProvider, FreeCountKind, FsOptions, LossyOemCpConverter};
use embedded_io_async::{Seek, SeekFrom, Write};

const FAT12_IMG: &str = "fat12.img";
const FAT16_IMG: &str = "fat16.img";
const FAT32_IMG: &str = "fat32.img";
const IMG_DIR: &str = "resources";
const TMP_DIR: &str = "tmp";
const TEST_STR: &str = "Hi there Rust programmer!\n";
const TEST_STR2: &str = "Rust is cool!\n";

type FileSystem = embedded_fatfs::FileSystem<
    embedded_io_adapters::tokio_1::FromTokio<tokio::fs::File>,
    ChronoTimeProvider,
    LossyOemCpConverter,
>;

async fn call_with_tmp_img<Fut: Future, F: Fn(String) -> Fut>(f: F, filename: &str, test_seq: u32) {
    let _ = env_logger::builder().is_test(true).try_init();
    let img_path = format!("{}/{}", IMG_DIR, filename);
    let tmp_path = format!("{}/{}-{}", TMP_DIR, test_seq, filename);
    fs::create_dir(TMP_DIR).await.ok();
    fs::copy(&img_path, &tmp_path).await.unwrap();
    f(tmp_path.clone()).await;
    fs::remove_file(tmp_path).await.unwrap();
}

async fn open_filesystem_rw(tmp_path: String) -> FileSystem {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&tmp_path)
        .await
        .unwrap();
    let options = FsOptions::new().update_accessed_date(true);
    FileSystem::new(file, options).await.unwrap()
}

async fn call_with_fs<Fut: Future, F: Fn(FileSystem) -> Fut>(f: F, filename: &str, test_seq: u32) {
    let callback = |tmp_path: String| async {
        let fs = open_filesystem_rw(tmp_path).await;
        f(fs).await;
    };
    call_with_tmp_img(&callback, filename, test_seq).await;
}

async fn test_write_short_file(fs: FileSystem) {
    let root_dir = fs.root_dir();
    let mut file = root_dir.open_file("short.txt").await.expect("open file");
    file.truncate().await.unwrap();
    file.write_all(&TEST_STR.as_bytes()).await.unwrap();
    file.flush().await.unwrap();
    file.seek(SeekFrom::Start(0)).await.unwrap();
    let buf = read_to_end(&mut file).await.unwrap();
    file.flush().await.unwrap(); // update access time
    assert_eq!(TEST_STR, str::from_utf8(&buf).unwrap());
}

#[tokio::test]
async fn test_write_file_fat12() {
    call_with_fs(test_write_short_file, FAT12_IMG, 1).await
}

#[tokio::test]
async fn test_write_file_fat16() {
    call_with_fs(test_write_short_file, FAT16_IMG, 1).await
}

#[tokio::test]
async fn test_write_file_fat32() {
    call_with_fs(test_write_short_file, FAT32_IMG, 1).await
}

async fn test_write_long_file(fs: FileSystem) {
    let root_dir = fs.root_dir();
    let mut file = root_dir.open_file("long.txt").await.expect("open file");
    file.truncate().await.unwrap();
    let test_str = TEST_STR.repeat(1000);
    file.write_all(&test_str.as_bytes()).await.unwrap();
    file.seek(SeekFrom::Start(0)).await.unwrap();
    file.flush().await.unwrap();
    let mut buf = read_to_end(&mut file).await.unwrap();
    assert_eq!(test_str, str::from_utf8(&buf).unwrap());
    file.seek(SeekFrom::Start(1234)).await.unwrap();
    file.truncate().await.unwrap();
    file.seek(SeekFrom::Start(0)).await.unwrap();
    buf.clear();
    buf = read_to_end(&mut file).await.unwrap();
    file.flush().await.unwrap(); // update access time
    assert_eq!(&test_str[..1234], str::from_utf8(&buf).unwrap());
}

#[tokio::test]
async fn test_write_long_file_fat12() {
    call_with_fs(test_write_long_file, FAT12_IMG, 2).await
}

#[tokio::test]
async fn test_write_long_file_fat16() {
    call_with_fs(test_write_long_file, FAT16_IMG, 2).await
}

#[tokio::test]
async fn test_write_long_file_fat32() {
    call_with_fs(test_write_long_file, FAT32_IMG, 2).await
}

async fn test_remove(fs: FileSystem) {
    let root_dir = fs.root_dir();
    assert!(root_dir.remove("very/long/path").await.is_err());
    let dir = root_dir.open_dir("very/long/path").await.unwrap();
    let mut names = dir
        .iter()
        .collect()
        .await
        .iter()
        .map(|r| r.as_ref().unwrap().file_name())
        .collect::<Vec<String>>();
    assert_eq!(names, [".", "..", "test.txt"]);
    root_dir.remove("very/long/path/test.txt").await.unwrap();
    names = dir
        .iter()
        .collect()
        .await
        .iter()
        .map(|r| r.as_ref().unwrap().file_name())
        .collect::<Vec<String>>();
    assert_eq!(names, [".", ".."]);
    assert!(root_dir.remove("very/long/path").await.is_ok());

    names = root_dir
        .iter()
        .collect()
        .await
        .iter()
        .map(|r| r.as_ref().unwrap().file_name())
        .collect::<Vec<String>>();
    assert_eq!(names, ["long.txt", "short.txt", "very", "very-long-dir-name"]);
    root_dir.remove("long.txt").await.unwrap();
    names = root_dir
        .iter()
        .collect()
        .await
        .iter()
        .map(|r| r.as_ref().unwrap().file_name())
        .collect::<Vec<String>>();
    assert_eq!(names, ["short.txt", "very", "very-long-dir-name"]);
}

#[tokio::test]
async fn test_remove_fat12() {
    call_with_fs(test_remove, FAT12_IMG, 3).await
}

#[tokio::test]
async fn test_remove_fat16() {
    call_with_fs(test_remove, FAT16_IMG, 3).await
}

#[tokio::test]
async fn test_remove_fat32() {
    call_with_fs(test_remove, FAT32_IMG, 3).await
}

async fn test_create_file(fs: FileSystem) {
    let root_dir = fs.root_dir();
    let dir = root_dir.open_dir("very/long/path").await.unwrap();
    let mut names = dir
        .iter()
        .collect()
        .await
        .iter()
        .map(|r| r.as_ref().unwrap().file_name())
        .collect::<Vec<String>>();
    assert_eq!(names, [".", "..", "test.txt"]);
    {
        // test some invalid names
        assert!(root_dir.create_file("very/long/path/:").await.is_err());
        assert!(root_dir.create_file("very/long/path/\0").await.is_err());
        // create file
        let mut file = root_dir
            .create_file("very/long/path/new-file-with-long-name.txt")
            .await
            .unwrap();
        file.write_all(&TEST_STR.as_bytes()).await.unwrap();
        file.flush().await.unwrap();
    }
    // check for dir entry
    names = dir
        .iter()
        .collect()
        .await
        .iter()
        .map(|r| r.as_ref().unwrap().file_name())
        .collect::<Vec<String>>();
    assert_eq!(names, [".", "..", "test.txt", "new-file-with-long-name.txt"]);
    names = dir
        .iter()
        .collect()
        .await
        .iter()
        .map(|r| r.as_ref().unwrap().short_file_name())
        .collect::<Vec<String>>();
    assert_eq!(names, [".", "..", "TEST.TXT", "NEW-FI~1.TXT"]);
    {
        // check contents
        let mut file = root_dir
            .open_file("very/long/path/new-file-with-long-name.txt")
            .await
            .unwrap();
        let buf = read_to_end(&mut file).await.unwrap();
        assert_eq!(&core::str::from_utf8(&buf).unwrap(), &TEST_STR);
    }
    // Create enough entries to allocate next cluster
    for i in 0..512 / 32 {
        let name = format!("test{}", i);
        dir.create_file(&name).await.unwrap();
    }
    names = dir
        .iter()
        .collect()
        .await
        .iter()
        .map(|r| r.as_ref().unwrap().file_name())
        .collect::<Vec<String>>();
    assert_eq!(names.len(), 4 + 512 / 32);
    // check creating existing file opens it
    {
        let mut file = root_dir
            .create_file("very/long/path/new-file-with-long-name.txt")
            .await
            .unwrap();
        let buf = read_to_end(&mut file).await.unwrap();
        assert_eq!(&core::str::from_utf8(&buf).unwrap(), &TEST_STR);
    }
    // check using create_file with existing directory fails
    assert!(root_dir.create_file("very").await.is_err());
}

#[tokio::test]
async fn test_create_file_fat12() {
    call_with_fs(test_create_file, FAT12_IMG, 4).await
}

#[tokio::test]
async fn test_create_file_fat16() {
    call_with_fs(test_create_file, FAT16_IMG, 4).await
}

#[tokio::test]
async fn test_create_file_fat32() {
    call_with_fs(test_create_file, FAT32_IMG, 4).await
}

async fn test_dot_only_names(fs: FileSystem) {
    let root_dir = fs.root_dir();
    for name in ["...", ". .", " ", "   "] {
        assert!(
            matches!(
                root_dir.create_file(name).await,
                Err(embedded_fatfs::Error::InvalidFileName)
            ),
            "create_file({name:?})"
        );
        assert!(
            matches!(
                root_dir.create_dir(name).await,
                Err(embedded_fatfs::Error::InvalidFileName)
            ),
            "create_dir({name:?})"
        );
    }
    // Leading dots and inner spaces are valid.
    root_dir.create_file(".hidden").await.unwrap().flush().await.unwrap();
    root_dir.create_file("a b.txt").await.unwrap().flush().await.unwrap();
}

#[tokio::test]
async fn test_dot_only_names_fat12() {
    call_with_fs(test_dot_only_names, FAT12_IMG, 9).await
}

#[tokio::test]
async fn test_dot_only_names_fat16() {
    call_with_fs(test_dot_only_names, FAT16_IMG, 9).await
}

#[tokio::test]
async fn test_dot_only_names_fat32() {
    call_with_fs(test_dot_only_names, FAT32_IMG, 9).await
}

async fn test_non_ascii_and_empty_names(fs: FileSystem) {
    let root_dir = fs.root_dir();
    // The first character takes more than one byte.
    root_dir.create_file("été.txt").await.unwrap().flush().await.unwrap();
    root_dir.create_dir("ü").await.unwrap();
    let names = root_dir
        .iter()
        .collect()
        .await
        .iter()
        .map(|r| r.as_ref().unwrap().file_name())
        .collect::<Vec<_>>();
    assert!(names.iter().any(|n| n == "été.txt"), "{names:?}");
    assert!(names.iter().any(|n| n == "ü"), "{names:?}");

    assert!(matches!(
        root_dir.create_file("").await,
        Err(embedded_fatfs::Error::InvalidFileNameLength)
    ));
    assert!(matches!(
        root_dir.create_dir("").await,
        Err(embedded_fatfs::Error::InvalidFileNameLength)
    ));
}

#[tokio::test]
async fn test_non_ascii_and_empty_names_fat12() {
    call_with_fs(test_non_ascii_and_empty_names, FAT12_IMG, 12).await
}

#[tokio::test]
async fn test_non_ascii_and_empty_names_fat16() {
    call_with_fs(test_non_ascii_and_empty_names, FAT16_IMG, 12).await
}

#[tokio::test]
async fn test_non_ascii_and_empty_names_fat32() {
    call_with_fs(test_non_ascii_and_empty_names, FAT32_IMG, 12).await
}

/// A full FAT12/16 root directory returns an error, and does not panic.
///
/// The image is in memory, because a file-backed disk makes the many directory scans slow.
async fn test_full_root_dir(filename: &str) {
    let _ = env_logger::builder().is_test(true).try_init();
    let bytes = fs::read(format!("{}/{}", IMG_DIR, filename)).await.unwrap();
    let disk = embedded_io_adapters::tokio_1::FromTokio::new(std::io::Cursor::new(bytes));
    let fs = embedded_fatfs::FileSystem::new(disk, FsOptions::new()).await.unwrap();
    let root_dir = fs.root_dir();
    let mut created = 0;
    let err = loop {
        match root_dir.create_file(&format!("F{created:05}.TXT")).await {
            Ok(_) => {}
            Err(err) => break err,
        }
        created += 1;
        assert!(created < 10_000, "root directory did not become full");
    };
    assert!(matches!(err, embedded_fatfs::Error::NotEnoughSpace), "{err:?}");
    assert!(created > 0);

    // The directory is still usable.
    let count = root_dir.iter().collect().await.iter().filter(|r| r.is_ok()).count();
    assert!(count >= created);
    root_dir.remove("F00000.TXT").await.unwrap();
    root_dir.create_file("AGAIN.TXT").await.unwrap().flush().await.unwrap();
}

#[tokio::test]
async fn test_full_root_dir_fat12() {
    test_full_root_dir(FAT12_IMG).await
}

#[tokio::test]
async fn test_full_root_dir_fat16() {
    test_full_root_dir(FAT16_IMG).await
}

async fn test_create_dir(fs: FileSystem) {
    let root_dir = fs.root_dir();
    let parent_dir = root_dir.open_dir("very/long/path").await.unwrap();
    let mut names = parent_dir
        .iter()
        .collect()
        .await
        .iter()
        .map(|r| r.as_ref().unwrap().file_name())
        .collect::<Vec<String>>();
    assert_eq!(names, [".", "..", "test.txt"]);
    {
        let subdir = root_dir
            .create_dir("very/long/path/new-dir-with-long-name")
            .await
            .unwrap();
        names = subdir
            .iter()
            .collect()
            .await
            .iter()
            .map(|r| r.as_ref().unwrap().file_name())
            .collect::<Vec<String>>();
        assert_eq!(names, [".", ".."]);
    }
    // check if new entry is visible in parent
    names = parent_dir
        .iter()
        .collect()
        .await
        .iter()
        .map(|r| r.as_ref().unwrap().file_name())
        .collect::<Vec<String>>();
    assert_eq!(names, [".", "..", "test.txt", "new-dir-with-long-name"]);
    {
        // Check if new directory can be opened and read
        let subdir = root_dir
            .open_dir("very/long/path/new-dir-with-long-name")
            .await
            .unwrap();
        names = subdir
            .iter()
            .collect()
            .await
            .iter()
            .map(|r| r.as_ref().unwrap().file_name())
            .collect::<Vec<String>>();
        assert_eq!(names, [".", ".."]);
    }
    // Check if '.' is alias for new directory
    {
        let subdir = root_dir
            .open_dir("very/long/path/new-dir-with-long-name/.")
            .await
            .unwrap();
        names = subdir
            .iter()
            .collect()
            .await
            .iter()
            .map(|r| r.as_ref().unwrap().file_name())
            .collect::<Vec<String>>();
        assert_eq!(names, [".", ".."]);
    }
    // Check if '..' is alias for parent directory
    {
        let subdir = root_dir
            .open_dir("very/long/path/new-dir-with-long-name/..")
            .await
            .unwrap();
        names = subdir
            .iter()
            .collect()
            .await
            .iter()
            .map(|r| r.as_ref().unwrap().file_name())
            .collect::<Vec<String>>();
        assert_eq!(names, [".", "..", "test.txt", "new-dir-with-long-name"]);
    }
    // check if creating existing directory returns it
    {
        let subdir = root_dir.create_dir("very").await.unwrap();
        names = subdir
            .iter()
            .collect()
            .await
            .iter()
            .map(|r| r.as_ref().unwrap().file_name())
            .collect::<Vec<String>>();
        assert_eq!(names, [".", "..", "long"]);
    }
    // check short names validity after create_dir
    {
        let subdir = root_dir.create_dir("test").await.unwrap();
        names = subdir
            .iter()
            .collect()
            .await
            .iter()
            .map(|r| r.as_ref().unwrap().short_file_name())
            .collect::<Vec<String>>();
        assert_eq!(names, [".", ".."]);
    }

    // check using create_dir with existing file fails
    assert!(root_dir.create_dir("very/long/path/test.txt").await.is_err());
}

#[tokio::test]
async fn test_create_dir_fat12() {
    call_with_fs(test_create_dir, FAT12_IMG, 5).await
}

#[tokio::test]
async fn test_create_dir_fat16() {
    call_with_fs(test_create_dir, FAT16_IMG, 5).await
}

#[tokio::test]
async fn test_create_dir_fat32() {
    call_with_fs(test_create_dir, FAT32_IMG, 5).await
}

async fn test_rename_file(fs: FileSystem) {
    let root_dir = fs.root_dir();
    let parent_dir = root_dir.open_dir("very/long/path").await.unwrap();
    let entries = parent_dir.iter().collect().await;
    let entries = entries.iter().map(|r| r.as_ref().unwrap()).collect::<Vec<_>>();
    let names = entries.iter().map(|r| r.file_name()).collect::<Vec<_>>();
    assert_eq!(names, [".", "..", "test.txt"]);
    assert_eq!(entries[2].len(), 14);
    let stats = fs.stats().await.unwrap();

    parent_dir
        .rename("test.txt", &parent_dir, "new-long-name.txt")
        .await
        .unwrap();
    let entries = parent_dir.iter().collect().await;
    let entries = entries.iter().map(|r| r.as_ref().unwrap()).collect::<Vec<_>>();
    let names = entries.iter().map(|r| r.file_name()).collect::<Vec<_>>();
    assert_eq!(names, [".", "..", "new-long-name.txt"]);
    assert_eq!(entries[2].len(), TEST_STR2.len() as u64);
    let mut file = parent_dir.open_file("new-long-name.txt").await.unwrap();
    let buf = read_to_end(&mut file).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(str::from_utf8(&buf).unwrap(), TEST_STR2);

    parent_dir
        .rename("new-long-name.txt", &root_dir, "moved-file.txt")
        .await
        .unwrap();
    let entries = root_dir.iter().collect().await;
    let entries = entries.iter().map(|r| r.as_ref().unwrap()).collect::<Vec<_>>();
    let names = entries.iter().map(|r| r.file_name()).collect::<Vec<_>>();
    assert_eq!(
        names,
        ["long.txt", "short.txt", "very", "very-long-dir-name", "moved-file.txt"]
    );
    assert_eq!(entries[4].len(), TEST_STR2.len() as u64);
    let mut file = root_dir.open_file("moved-file.txt").await.unwrap();
    let buf = read_to_end(&mut file).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(str::from_utf8(&buf).unwrap(), TEST_STR2);

    assert!(root_dir.rename("moved-file.txt", &root_dir, "short.txt").await.is_err());
    let entries = root_dir.iter().collect().await;
    let entries = entries.iter().map(|r| r.as_ref().unwrap()).collect::<Vec<_>>();
    let names = entries.iter().map(|r| r.file_name()).collect::<Vec<_>>();
    assert_eq!(
        names,
        ["long.txt", "short.txt", "very", "very-long-dir-name", "moved-file.txt"]
    );

    assert!(root_dir
        .rename("moved-file.txt", &root_dir, "moved-file.txt")
        .await
        .is_ok());

    let new_stats = fs.stats().await.unwrap();
    assert_eq!(new_stats.free_clusters(), stats.free_clusters());
}

#[tokio::test]
async fn test_rename_file_fat12() {
    call_with_fs(test_rename_file, FAT12_IMG, 6).await
}

#[tokio::test]
async fn test_rename_file_fat16() {
    call_with_fs(test_rename_file, FAT16_IMG, 6).await
}

#[tokio::test]
async fn test_rename_file_fat32() {
    call_with_fs(test_rename_file, FAT32_IMG, 6).await
}

async fn test_new_from_context_rejects_stale_context(fs: FileSystem) {
    let root_dir = fs.root_dir();
    let file = root_dir.open_file("short.txt").await.unwrap();
    let stale = file.close().await.unwrap();

    let mut file = root_dir.open_file("short.txt").await.unwrap();
    file.seek(SeekFrom::End(0)).await.unwrap();
    file.write_all(TEST_STR.as_bytes()).await.unwrap();
    let fresh = file.close().await.unwrap();

    // The size on the disk changed, so the old context does not match.
    assert!(embedded_fatfs::File::new_from_context(stale, &fs).await.is_err());
    let mut file = embedded_fatfs::File::new_from_context(fresh, &fs).await.unwrap();
    file.flush().await.unwrap();

    // A deleted file does not match either.
    let file = root_dir.open_file("short.txt").await.unwrap();
    let context = file.close().await.unwrap();
    root_dir.remove("short.txt").await.unwrap();
    assert!(embedded_fatfs::File::new_from_context(context, &fs).await.is_err());
}

#[tokio::test]
async fn test_new_from_context_rejects_stale_context_fat12() {
    call_with_fs(test_new_from_context_rejects_stale_context, FAT12_IMG, 11).await
}

#[tokio::test]
async fn test_new_from_context_rejects_stale_context_fat16() {
    call_with_fs(test_new_from_context_rejects_stale_context, FAT16_IMG, 11).await
}

#[tokio::test]
async fn test_new_from_context_rejects_stale_context_fat32() {
    call_with_fs(test_new_from_context_rejects_stale_context, FAT32_IMG, 11).await
}

/// A simple wrapper around `mem::forget`, to make clear why we drop the FS.
const fn drop_fs_leaving_it_dirty<T>(fs: T) {
    core::mem::forget(fs)
}

/// The dirty flag is set by an unclean shutdown, and only an unmount clears it.
///
/// Dropping the filesystem without unmounting must set the flag, remounting must not clear it, and neither rebuilding
/// the free-cluster count nor flushing may either: a dirty mount distrusts the count in FSInfo, so `stats` scans the
/// whole FAT to rebuild it, and that scan says nothing about the metadata the flag is actually warning about.
async fn test_dirty_flag_survives_stats(tmp_path: String) {
    // Open filesystem, make change, and forget it - should become dirty
    let fs = open_filesystem_rw(tmp_path.clone()).await;
    let status_flags = fs.read_status_flags().await.unwrap();
    assert_eq!(status_flags.dirty(), false);
    assert_eq!(status_flags.io_error(), false);
    fs.root_dir().create_file("abc.txt").await.unwrap();
    drop_fs_leaving_it_dirty(fs);

    // Check if volume is dirty now
    let fs = open_filesystem_rw(tmp_path.clone()).await;
    let status_flags = fs.read_status_flags().await.unwrap();
    assert_eq!(status_flags.dirty(), true);
    assert_eq!(status_flags.io_error(), false);

    fs.stats().await.unwrap();
    fs.flush().await.unwrap();
    assert_eq!(fs.read_status_flags().await.unwrap().dirty(), true);
    fs.unmount().await.unwrap();

    // Make sure remounting does not clear the dirty flag
    let fs = open_filesystem_rw(tmp_path).await;
    let status_flags = fs.read_status_flags().await.unwrap();
    assert_eq!(status_flags.dirty(), true);
    assert_eq!(status_flags.io_error(), false);
}

#[tokio::test]
async fn test_dirty_flag_survives_stats_fat12() {
    call_with_tmp_img(test_dirty_flag_survives_stats, FAT12_IMG, 7).await
}

#[tokio::test]
async fn test_dirty_flag_survives_stats_fat16() {
    call_with_tmp_img(test_dirty_flag_survives_stats, FAT16_IMG, 7).await
}

#[tokio::test]
async fn test_dirty_flag_survives_stats_fat32() {
    call_with_tmp_img(test_dirty_flag_survives_stats, FAT32_IMG, 7).await
}

/// The dirty flag has to be set in both copies of the boot sector.
async fn test_dirty_flag_is_also_set_in_backup_boot_sector(tmp_path: String) {
    /// Offset of the dirty flag within a FAT32 boot sector, and of `BPB_BkBootSec` within the BPB.
    const DIRTY_FLAG: usize = 0x41;
    const BACKUP_BOOT_SECTOR: usize = 50;

    let before = fs::read(&tmp_path).await.unwrap();
    let bytes_per_sector = u16::from_le_bytes([before[11], before[12]]) as usize;
    let backup =
        u16::from_le_bytes([before[BACKUP_BOOT_SECTOR], before[BACKUP_BOOT_SECTOR + 1]]) as usize * bytes_per_sector;
    assert_ne!(backup, 0, "the fixture should have a backup boot sector");
    assert_eq!(before[DIRTY_FLAG] & 1, 0, "the fixture should start clean");

    // Leave the volume dirty by dropping a mutated filesystem without unmounting
    let fs = open_filesystem_rw(tmp_path.clone()).await;
    fs.root_dir().create_file("abc.txt").await.unwrap();
    drop_fs_leaving_it_dirty(fs);

    let after = fs::read(&tmp_path).await.unwrap();
    assert_eq!(after[DIRTY_FLAG] & 1, 1, "the boot sector was not marked dirty");
    assert_eq!(
        after[backup + DIRTY_FLAG] & 1,
        1,
        "the backup boot sector still says the volume is clean"
    );
}

/// FAT32 only, FAT12 and FAT16 have no backup boot sector to disagree with.
#[tokio::test]
async fn test_dirty_flag_is_also_set_in_backup_boot_sector_fat32() {
    call_with_tmp_img(test_dirty_flag_is_also_set_in_backup_boot_sector, FAT32_IMG, 10).await
}

async fn test_multiple_files_in_directory(fs: FileSystem) {
    let dir = fs.root_dir().create_dir("/TMP").await.unwrap();
    for i in 0..8 {
        let name = format!("T{}.TXT", i);
        let mut file = dir.create_file(&name).await.unwrap();
        file.write_all(TEST_STR.as_bytes()).await.unwrap();
        file.flush().await.unwrap();

        let files = dir.iter().collect().await;
        let files = files.iter().map(|r| r.as_ref().unwrap()).collect::<Vec<_>>();
        let file = files.iter().find(|e| e.file_name() == name).unwrap();
        assert_eq!(TEST_STR.len() as u64, file.len(), "Wrong file len on iteration {}", i);
    }
}

#[tokio::test]
async fn test_multiple_files_in_directory_fat12() {
    call_with_fs(&test_multiple_files_in_directory, FAT12_IMG, 8).await
}

#[tokio::test]
async fn test_multiple_files_in_directory_fat16() {
    call_with_fs(&test_multiple_files_in_directory, FAT16_IMG, 8).await
}

#[tokio::test]
async fn test_multiple_files_in_directory_fat32() {
    call_with_fs(&test_multiple_files_in_directory, FAT32_IMG, 8).await
}

async fn test_preallocate(fs: FileSystem) {
    let root_dir = fs.root_dir();
    let cluster_size = fs.cluster_size() as usize;
    let before = fs.stats().await.unwrap().free_clusters();
    let mut file = root_dir.create_file("prealloc.txt").await.unwrap();

    // Allocates the clusters but keeps the size and the position.
    file.preallocate((cluster_size * 3) as u64).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), before - 3);
    assert_eq!(file.seek(SeekFrom::Current(0)).await.unwrap(), 0);
    assert_eq!(file.seek(SeekFrom::End(0)).await.unwrap(), 0);

    // Asking for less than the chain covers, or growing it, allocates only what is missing.
    file.preallocate(1).await.unwrap();
    file.preallocate((cluster_size * 4) as u64).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), before - 4);

    // Writes go into the allocated clusters and allocate none.
    let data = vec![b'a'; cluster_size * 2 + 1];
    file.seek(SeekFrom::Start(0)).await.unwrap();
    file.write_all(&data).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), before - 4);
    assert_eq!(file.seek(SeekFrom::End(0)).await.unwrap(), data.len() as u64);

    // The chain is counted from the position, inside a cluster or on a boundary.
    file.preallocate((cluster_size * 4) as u64).await.unwrap();
    file.seek(SeekFrom::Start(cluster_size as u64)).await.unwrap();
    file.preallocate((cluster_size * 4) as u64).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), before - 4);
    file.preallocate((cluster_size * 5) as u64).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), before - 5);
    file.seek(SeekFrom::End(0)).await.unwrap();
    file.truncate().await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), before - 3);
    file.preallocate((cluster_size * 4) as u64).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), before - 4);

    // Truncating at the end of the data gives the unused clusters back.
    file.truncate().await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), before - 3);
    file.close().await.unwrap();
    let mut file = root_dir.open_file("prealloc.txt").await.unwrap();
    assert_eq!(read_to_end(&mut file).await.unwrap(), data);
    file.flush().await.unwrap();

    // Asking for more than the volume has keeps what it got; truncating gives it back.
    let free = fs.stats().await.unwrap().free_clusters();
    let too_much = (cluster_size * (free as usize + 3 + 1)) as u64;
    assert!(matches!(
        file.preallocate(too_much).await,
        Err(embedded_fatfs::Error::NotEnoughSpace)
    ));
    file.flush().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), 0);
    file.truncate().await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), free);
    file.seek(SeekFrom::Start(0)).await.unwrap();
    assert_eq!(read_to_end(&mut file).await.unwrap(), data);
}

async fn test_free_clusters(fs: FileSystem) {
    // FAT12 and FAT16 have no FS Information Sector, so the count is not known until counted.
    let known = fs.free_clusters();
    let counted = fs.stats().await.unwrap().free_clusters();
    let free = |count, kind| Some(embedded_fatfs::FreeClusters { count, kind });
    // A stored count is trusted by stats(), but only a count made on this mount is "counted".
    let kind = match fs.fat_type() {
        embedded_fatfs::FatType::Fat32 => FreeCountKind::Stored,
        _ => FreeCountKind::Counted,
    };
    match fs.fat_type() {
        embedded_fatfs::FatType::Fat32 => assert_eq!(known, free(counted, kind)),
        _ => assert_eq!(known, None),
    }
    assert_eq!(fs.free_clusters(), free(counted, kind));
    assert_eq!(fs.total_clusters(), fs.stats().await.unwrap().total_clusters());
    // Allocation keeps the known count up to date.
    let mut file = fs.root_dir().create_file("hint.txt").await.unwrap();
    file.write_all(&vec![0; fs.cluster_size() as usize]).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.free_clusters(), free(counted - 1, kind));
    while !fs.count_free_clusters_step(1 << 20).await.unwrap() {}
    assert_eq!(fs.free_clusters(), free(counted - 1, FreeCountKind::Counted));
}

#[tokio::test]
async fn test_free_clusters_fat12() {
    call_with_fs(test_free_clusters, FAT12_IMG, 14).await
}

#[tokio::test]
async fn test_free_clusters_fat16() {
    call_with_fs(test_free_clusters, FAT16_IMG, 14).await
}

#[tokio::test]
async fn test_free_clusters_fat32() {
    call_with_fs(test_free_clusters, FAT32_IMG, 14).await
}

async fn write_clusters(fs: &FileSystem, name: &str, clusters: usize) {
    let mut file = fs.root_dir().create_file(name).await.unwrap();
    file.write_all(&vec![b'x'; fs.cluster_size() as usize * clusters])
        .await
        .unwrap();
    file.flush().await.unwrap();
}

async fn test_free_count_steps(tmp_path: String) {
    // A volume left without an unmount: its FS Information Sector still holds the count from
    // before the file was written.
    let fs = open_filesystem_rw(tmp_path.clone()).await;
    let before = fs.stats().await.unwrap().free_clusters();
    fs.flush().await.unwrap();
    write_clusters(&fs, "a.txt", 3).await;
    drop(fs);

    let fs = open_filesystem_rw(tmp_path.clone()).await;
    let fat32 = fs.fat_type() == embedded_fatfs::FatType::Fat32;
    let estimate = fat32.then_some(embedded_fatfs::FreeClusters {
        count: before,
        kind: FreeCountKind::Estimate,
    });
    assert_eq!(fs.free_clusters(), estimate);

    // Clusters allocated and freed between steps, on both sides of where the count stopped.
    let mut steps = 1;
    let mut done = fs.count_free_clusters_step(16).await.unwrap();
    assert_eq!(done, !fat32);
    write_clusters(&fs, "b.txt", 2).await;
    fs.root_dir().remove("a.txt").await.unwrap();
    while !done {
        done = fs.count_free_clusters_step(16).await.unwrap();
        steps += 1;
    }
    assert!(!fat32 || steps > 2);
    let exact = Some(embedded_fatfs::FreeClusters {
        count: before - 2,
        kind: FreeCountKind::Counted,
    });
    assert_eq!(fs.free_clusters(), exact);
    assert!(fs.count_free_clusters_step(16).await.unwrap());

    // The counted count is written back. A volume mounted dirty stays dirty, so the next mount
    // takes it as an estimate again, and a count from scratch agrees.
    fs.unmount().await.unwrap();
    let fs = open_filesystem_rw(tmp_path).await;
    if fat32 {
        let estimate = Some(embedded_fatfs::FreeClusters {
            count: before - 2,
            kind: FreeCountKind::Estimate,
        });
        assert_eq!(fs.free_clusters(), estimate);
    }
    while !fs.count_free_clusters_step(1 << 20).await.unwrap() {}
    assert_eq!(fs.free_clusters(), exact);
}

#[tokio::test]
async fn test_free_count_steps_fat12() {
    call_with_tmp_img(test_free_count_steps, FAT12_IMG, 16).await
}

#[tokio::test]
async fn test_free_count_steps_fat32() {
    call_with_tmp_img(test_free_count_steps, FAT32_IMG, 16).await
}

async fn test_full_volume(fs: FileSystem) {
    let cluster_size = fs.cluster_size() as usize;
    while !fs.count_free_clusters_step(1 << 20).await.unwrap() {}
    let free = fs.stats().await.unwrap().free_clusters() as usize;
    let mut file = fs.root_dir().create_file("full.txt").await.unwrap();
    file.write_all(&vec![b'x'; cluster_size * free]).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.free_clusters().map(|free| free.count), Some(0));
    assert!(matches!(
        file.write_all(&[b'y']).await,
        Err(embedded_fatfs::Error::NotEnoughSpace)
    ));
    // Space freed by a truncate is found again, at clusters before the next free cluster hint.
    file.seek(SeekFrom::Start(0)).await.unwrap();
    file.truncate().await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.free_clusters().map(|free| free.count), Some(free as u32));
    file.write_all(&vec![b'z'; cluster_size]).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.free_clusters().map(|free| free.count), Some(free as u32 - 1));
}

#[tokio::test]
async fn test_full_volume_fat12() {
    call_with_fs(test_full_volume, FAT12_IMG, 17).await
}

async fn test_preallocate_edges(fs: FileSystem) {
    let root_dir = fs.root_dir();
    let cluster_size = fs.cluster_size() as usize;
    let before = fs.stats().await.unwrap().free_clusters();

    // An empty file truncated at 0 gives its preallocated chain back.
    let mut file = root_dir.create_file("empty.txt").await.unwrap();
    file.preallocate((cluster_size * 2) as u64).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), before - 2);
    file.seek(SeekFrom::End(0)).await.unwrap();
    file.truncate().await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), before);

    // A file reopened later, as after a power loss, appends into the clusters still allocated.
    let data = vec![b'x'; cluster_size + 1];
    let mut file = root_dir.create_file("reopen.txt").await.unwrap();
    file.write_all(&data).await.unwrap();
    file.preallocate((cluster_size * 4) as u64).await.unwrap();
    file.close().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), before - 4);
    let mut file = root_dir.open_file("reopen.txt").await.unwrap();
    file.seek(SeekFrom::End(0)).await.unwrap();
    file.write_all(&vec![b'y'; cluster_size * 2]).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(fs.stats().await.unwrap().free_clusters(), before - 4);

    // A length past the largest file size is refused.
    assert!(matches!(
        file.preallocate(u64::from(u32::MAX) + 1).await,
        Err(embedded_fatfs::Error::InvalidInput)
    ));
}

#[tokio::test]
async fn test_preallocate_edges_fat12() {
    call_with_fs(test_preallocate_edges, FAT12_IMG, 15).await
}

#[tokio::test]
async fn test_preallocate_edges_fat32() {
    call_with_fs(test_preallocate_edges, FAT32_IMG, 15).await
}

#[tokio::test]
async fn test_preallocate_fat12() {
    call_with_fs(test_preallocate, FAT12_IMG, 13).await
}

#[tokio::test]
async fn test_preallocate_fat16() {
    call_with_fs(test_preallocate, FAT16_IMG, 13).await
}

#[tokio::test]
async fn test_preallocate_fat32() {
    call_with_fs(test_preallocate, FAT32_IMG, 13).await
}

async fn read_to_end<IO: embedded_io_async::Read>(io: &mut IO) -> Result<Vec<u8>, IO::Error> {
    let mut buf = Vec::new();
    loop {
        let mut tmp = [0; 256];
        match io.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => buf.extend(&tmp[..n]),
            Err(e) => return Err(e),
        }
    }

    Ok(buf)
}
