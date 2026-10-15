//! Integration tests on real FAT images built with mkfs.vfat and mtools.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::*;

/// 2024-06-01 12:34:56 UTC (even seconds: FAT has 2-second resolution).
const STAMP: u64 = 1_717_245_296;

/// Copies a fixture to `dir/name` with a fixed modification time.
fn staged(dir: &Path, fixture_name: &str, name: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    fs::copy(fixture(fixture_name), &p).unwrap();
    let f = fs::File::options().write(true).open(&p).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_secs(STAMP))
        .unwrap();
    p
}

fn deleted_tree_case(fat: u8, size_kib: u32) {
    if !have_fat_tools() {
        return;
    }
    let w = workdir(&format!("tree{fat}"));
    let stage = w.join("stage");
    fs::create_dir_all(&stage).unwrap();
    let img = w.join("card.img");
    let i = img.to_str().unwrap();
    mkfs(&img, fat, size_kib);
    tool(
        "mmd",
        &[
            "-i",
            i,
            "::/DCIM",
            "::/DCIM/100CANON",
            "::/Old Stuff",
            "::/Old Stuff/Songs",
        ],
    );
    let beach = staged(&stage, "photo.jpg", "Holiday at the beach.jpg");
    let img2 = staged(&stage, "photo2.jpg", "IMG_0002.JPG");
    let wav = staged(&stage, "sound.wav", "Sound Recording.wav");
    let mp4 = staged(&stage, "clip.mp4", "clip.mp4");
    let mp3 = staged(&stage, "song.mp3", "A song with a long name.mp3");
    let keep = staged(&stage, "image.png", "KEEP.PNG");
    mcopy(i, &beach, "DCIM/100CANON/Holiday at the beach.jpg");
    mcopy(i, &img2, "DCIM/100CANON/IMG_0002.JPG");
    mcopy(i, &wav, "Sound Recording.wav");
    mcopy(i, &mp4, "Old Stuff/clip.mp4");
    mcopy(i, &mp3, "Old Stuff/Songs/A song with a long name.mp3");
    mcopy(i, &keep, "KEEP.PNG");
    tool(
        "mdel",
        &[
            "-i",
            i,
            "::/DCIM/100CANON/Holiday at the beach.jpg",
            "::/DCIM/100CANON/IMG_0002.JPG",
            "::/Sound Recording.wav",
        ],
    );
    tool("mdeltree", &["-i", i, "::/Old Stuff"]);

    let (rc, rows) = list_json(&w, "card.img", &[]);
    assert_eq!(rc, 0);
    let expected = [
        ("/DCIM/100CANON/Holiday at the beach.jpg", "photo.jpg"),
        // No long name: the lost first letter becomes '_'.
        ("/DCIM/100CANON/_MG_0002.JPG", "photo2.jpg"),
        ("/Sound Recording.wav", "sound.wav"),
        // Files inside a deleted directory (and a deleted subdirectory of it).
        // `clip.mp4` is stored as an 8.3 name with lower-case flags and no
        // long name, so its first letter is lost too.
        ("/Old Stuff/_lip.mp4", "clip.mp4"),
        ("/Old Stuff/Songs/A song with a long name.mp3", "song.mp3"),
    ];
    assert_eq!(rows.len(), expected.len(), "{rows:#?}");
    for (path, fx) in expected {
        let r = row(&rows, path);
        assert_eq!(r["status"], "good", "{path}");
        assert_eq!(r["signature"], "ok", "{path}");
        assert_eq!(r["deleted"], true);
        assert_eq!(
            r["size"].as_u64().unwrap(),
            fs::metadata(fixture(fx)).unwrap().len()
        );
    }
    assert!(!rows.iter().any(|r| r["path"] == "/KEEP.PNG"));

    // --all adds existing files; --type filters by category.
    let (_, all) = list_json(&w, "card.img", &["--all"]);
    assert_eq!(row(&all, "/KEEP.PNG")["status"], "existing");
    let (_, photos) = list_json(&w, "card.img", &["--type", "photo"]);
    assert_eq!(photos.len(), 2);

    // Undelete restores names, folders, contents and timestamps.
    let o = sdrescue(&w, &["undelete", "card.img", "--out", "rec"]);
    assert_eq!(code(&o), 0, "{}", text(&o));
    for (path, fx) in expected {
        let got = w.join("rec").join(path.trim_start_matches('/'));
        assert_eq!(
            fs::read(&got).unwrap_or_else(|e| panic!("{}: {e}", got.display())),
            fs::read(fixture(fx)).unwrap(),
            "{path}"
        );
        let mtime = fs::metadata(&got).unwrap().modified().unwrap();
        let diff = mtime
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .abs_diff(STAMP);
        assert!(diff <= 2, "{path}: mtime off by {diff}s");
    }
    let report = fs::read_to_string(w.join("rec/report.csv")).unwrap();
    let lines: Vec<&str> = report.lines().collect();
    assert_eq!(
        lines[0],
        "original_path,recovered_path,size,status,signature"
    );
    assert_eq!(lines.len(), 1 + expected.len());
    assert!(report.contains("/Sound Recording.wav,Sound Recording.wav,24044,good,ok"));

    // A second run never overwrites: files get a numeric suffix.
    let o = sdrescue(&w, &["undelete", "card.img", "--out", "rec"]);
    assert_eq!(code(&o), 0, "{}", text(&o));
    assert!(w.join("rec/Sound Recording (2).wav").exists());
    assert!(w.join("rec/report (2).csv").exists());
}

#[test]
fn fat12_deleted_files_folders_and_long_names() {
    deleted_tree_case(12, 2048);
}

#[test]
fn fat16_deleted_files_folders_and_long_names() {
    deleted_tree_case(16, 16 * 1024);
}

#[test]
fn fat32_deleted_files_folders_and_long_names() {
    deleted_tree_case(32, 40_000);
}

#[test]
fn file_overwritten_after_deletion() {
    if !have_fat_tools() {
        return;
    }
    let w = workdir("overwritten");
    let img = w.join("card.img");
    let i = img.to_str().unwrap();
    mkfs(&img, 16, 16 * 1024);
    // The new file goes into another folder so it does not reuse the deleted
    // directory entry, only the freed clusters.
    tool("mmd", &["-i", i, "::/NEW"]);
    mcopy(i, &fixture("photo.jpg"), "First photo.jpg");
    tool("mdel", &["-i", i, "::/First photo.jpg"]);
    mcopy(i, &fixture("image.png"), "NEW/second.png");
    let (_, rows) = list_json(&w, "card.img", &[]);
    let r = row(&rows, "/First photo.jpg");
    let status = r["status"].as_str().unwrap();
    assert!(
        status == "overwritten" || status == "partial",
        "status {status}"
    );
    if status == "overwritten" {
        // The first cluster now holds PNG data.
        assert_eq!(r["signature"], "mismatch");
        // Skipped by default...
        let o = sdrescue(&w, &["undelete", "card.img", "--out", "a"]);
        assert_eq!(code(&o), 2, "{}", text(&o));
        assert!(!w.join("a/First photo.jpg").exists());
        // ...unless asked for.
        let o = sdrescue(
            &w,
            &[
                "undelete",
                "card.img",
                "--out",
                "b",
                "--include-overwritten",
            ],
        );
        assert_eq!(code(&o), 0, "{}", text(&o));
        assert!(w.join("b/First photo.jpg").exists());
    }
}

fn write_mbr(img: &Path, part_type: u8, lba: u32, sectors: u32) {
    let mut s = vec![0u8; 512];
    let e = &mut s[446..462];
    e[0] = 0x80;
    e[4] = part_type;
    e[8..12].copy_from_slice(&lba.to_le_bytes());
    e[12..16].copy_from_slice(&sectors.to_le_bytes());
    s[510] = 0x55;
    s[511] = 0xAA;
    let f = fs::File::create(img).unwrap();
    f.set_len(u64::from(lba + sectors) * 512).unwrap();
    use std::os::unix::fs::FileExt;
    f.write_all_at(&s, 0).unwrap();
}

#[test]
fn mbr_partition_table_in_front_of_the_filesystem() {
    if !have_fat_tools() {
        return;
    }
    let w = workdir("mbr");
    let img = w.join("disk.img");
    let sectors = 16 * 1024 * 2; // 16 MiB partition
    write_mbr(&img, 0x0E, 2048, sectors);
    tool(
        "mkfs.vfat",
        &[
            "-F",
            "16",
            "--offset",
            "2048",
            img.to_str().unwrap(),
            &(sectors / 2).to_string(),
        ],
    );
    let spec = format!("{}@@1M", img.display());
    mcopy(&spec, &fixture("photo.jpg"), "Birthday cake.jpg");
    tool("mdel", &["-i", &spec, "::/Birthday cake.jpg"]);

    let o = sdrescue(&w, &["info", "disk.img"]);
    let out = text(&o);
    assert_eq!(code(&o), 0, "{out}");
    assert!(out.contains("Partition table:   MBR"), "{out}");
    assert!(out.contains("<- analysed"), "{out}");
    assert!(out.contains("Filesystem at offset 1048576: FAT16"), "{out}");

    let (rc, rows) = list_json(&w, "disk.img", &[]);
    assert_eq!(rc, 0);
    assert_eq!(row(&rows, "/Birthday cake.jpg")["status"], "good");
    // --offset gives the same result.
    let (_, rows2) = list_json(&w, "disk.img", &["--offset", "1M"]);
    assert_eq!(rows2.len(), 1);

    let o = sdrescue(&w, &["undelete", "disk.img", "--out", "rec"]);
    assert_eq!(code(&o), 0, "{}", text(&o));
    assert_eq!(
        fs::read(w.join("rec/Birthday cake.jpg")).unwrap(),
        fs::read(fixture("photo.jpg")).unwrap()
    );
}

const MEDIA: [&str; 8] = [
    "photo.jpg",
    "photo2.jpg",
    "image.png",
    "anim.gif",
    "sound.wav",
    "song.mp3",
    "voice.ogg",
    "clip.mp4",
];

#[test]
fn quick_reformat_list_finds_nothing_but_carve_recovers() {
    if !have_fat_tools() {
        return;
    }
    let w = workdir("reformat");
    let img = w.join("card.img");
    let i = img.to_str().unwrap();
    mkfs(&img, 32, 40_000);
    tool("mmd", &["-i", i, "::/DCIM"]);
    for m in MEDIA {
        mcopy(i, &fixture(m), &format!("DCIM/{m}"));
    }
    // Quick format over the existing image: new boot sector, FATs and root only.
    tool("mkfs.vfat", &["-F", "32", i]);

    let (rc, rows) = list_json(&w, "card.img", &[]);
    assert_eq!(rc, 2, "nothing should be listed: {rows:#?}");
    assert!(rows.is_empty());

    let o = sdrescue(&w, &["carve", "card.img", "--out", "carved"]);
    assert_eq!(code(&o), 0, "{}", text(&o));
    let mut carved: Vec<Vec<u8>> = Vec::new();
    for dir in fs::read_dir(w.join("carved")).unwrap() {
        let dir = dir.unwrap().path();
        if dir.is_dir() {
            for f in fs::read_dir(&dir).unwrap() {
                carved.push(fs::read(f.unwrap().path()).unwrap());
            }
        }
    }
    for m in MEDIA {
        let want = fs::read(fixture(m)).unwrap();
        assert!(
            carved.contains(&want),
            "{m} was not carved byte-for-byte; report:\n{}",
            fs::read_to_string(w.join("carved/report.csv")).unwrap()
        );
    }
    assert_eq!(carved.len(), MEDIA.len(), "unexpected extra carved files");
    let report = fs::read_to_string(w.join("carved/report.csv")).unwrap();
    assert!(report.starts_with("offset,length,type,end,truncated,file,note"));
    assert!(w.join("carved/jpg").is_dir());

    // Type filter.
    let o = sdrescue(
        &w,
        &["carve", "card.img", "--out", "audio", "--type", "audio"],
    );
    assert_eq!(code(&o), 0, "{}", text(&o));
    let mut kinds: Vec<String> = fs::read_dir(w.join("audio"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "report.csv")
        .collect();
    kinds.sort();
    assert_eq!(kinds, ["mp3", "ogg", "wav"]);
}

#[test]
fn carve_skips_allocated_files_by_default() {
    if !have_fat_tools() {
        return;
    }
    let w = workdir("carvefree");
    let img = w.join("card.img");
    let i = img.to_str().unwrap();
    mkfs(&img, 16, 16 * 1024);
    mcopy(i, &fixture("photo.jpg"), "kept.jpg");
    mcopy(i, &fixture("image.png"), "gone.png");
    tool("mdel", &["-i", i, "::/gone.png"]);
    let o = sdrescue(&w, &["carve", "card.img", "--out", "free"]);
    assert_eq!(code(&o), 0, "{}", text(&o));
    assert!(w.join("free/png").is_dir());
    assert!(!w.join("free/jpg").exists(), "existing file was carved");
    let o = sdrescue(&w, &["carve", "card.img", "--out", "all", "--all-space"]);
    assert_eq!(code(&o), 0, "{}", text(&o));
    assert!(w.join("all/jpg").is_dir());
}

#[test]
fn empty_card_exits_with_2() {
    if !have_fat_tools() {
        return;
    }
    let w = workdir("empty");
    mkfs(&w.join("card.img"), 16, 16 * 1024);
    assert_eq!(code(&sdrescue(&w, &["list", "card.img"])), 2);
    assert_eq!(
        code(&sdrescue(&w, &["undelete", "card.img", "--out", "u"])),
        2
    );
    assert_eq!(code(&sdrescue(&w, &["carve", "card.img", "--out", "c"])), 2);
    assert_eq!(code(&sdrescue(&w, &["info", "card.img"])), 0);
}

#[test]
fn image_command_copies_and_logs_sha256() {
    if !have_fat_tools() {
        return;
    }
    use sha2::{Digest, Sha256};
    let w = workdir("image");
    mkfs(&w.join("card.img"), 16, 16 * 1024);
    let o = sdrescue(&w, &["image", "card.img", "copy.img"]);
    assert_eq!(code(&o), 0, "{}", text(&o));
    let data = fs::read(w.join("card.img")).unwrap();
    assert_eq!(fs::read(w.join("copy.img")).unwrap(), data);
    let hash: String = Sha256::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let log = fs::read_to_string(w.join("copy.img.log")).unwrap();
    assert!(log.contains(&format!("sha256: {hash}")), "{log}");
    assert!(log.contains("bad sectors: 0"));
    assert!(log.contains("sector size: 512 bytes"));
    // Refuses to overwrite an existing image.
    let before = SystemTime::now();
    let o = sdrescue(&w, &["image", "card.img", "copy.img"]);
    assert_eq!(code(&o), 1);
    assert!(
        fs::metadata(w.join("copy.img"))
            .unwrap()
            .modified()
            .unwrap()
            <= before
    );
}
