//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Path of the compiled `sdrescue` binary.
pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sdrescue")
}

/// Runs sdrescue with `args` in `cwd`.
pub fn sdrescue(cwd: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run sdrescue")
}

/// Exit code of a finished command.
pub fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

/// Stdout + stderr as text, for assertion messages.
pub fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// A fresh, empty directory under Cargo's per-target temp dir.
pub fn workdir(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("create workdir");
    d
}

/// The committed sample media.
pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn find_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/sbin"].map(PathBuf::from))
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// `mkfs.vfat` and `mcopy` are needed to build FAT images. Returns false (skip)
/// only when `SDRESCUE_SKIP_FAT_TESTS` is set; otherwise missing tools fail the test.
pub fn have_fat_tools() -> bool {
    let ok = find_tool("mkfs.vfat").is_some() && find_tool("mcopy").is_some();
    if ok {
        return true;
    }
    if std::env::var_os("SDRESCUE_SKIP_FAT_TESTS").is_some() {
        eprintln!("skipping: mkfs.vfat/mtools not installed");
        return false;
    }
    panic!(
        "these tests build FAT images with mkfs.vfat and mtools.\n\
         Install them (Ubuntu/WSL: sudo apt install dosfstools mtools) or set SDRESCUE_SKIP_FAT_TESTS=1"
    );
}

/// Runs an external tool and panics with its output on failure.
pub fn tool(name: &str, args: &[&str]) {
    let exe = find_tool(name).unwrap_or_else(|| PathBuf::from(name));
    let o = Command::new(exe)
        .args(args)
        .env("MTOOLS_SKIP_CHECK", "1")
        .output()
        .unwrap_or_else(|e| panic!("cannot run {name}: {e}"));
    assert!(o.status.success(), "{name} {args:?} failed:\n{}", text(&o));
}

/// Creates a FAT image of `size_kib` KiB with mkfs.vfat.
pub fn mkfs(img: &Path, fat: u8, size_kib: u32) {
    tool(
        "mkfs.vfat",
        &[
            "-C",
            "-F",
            &fat.to_string(),
            "-n",
            "SDTEST",
            img.to_str().unwrap(),
            &size_kib.to_string(),
        ],
    );
}

/// `mcopy -m` (keep timestamps) of a host file to `::/<dest>` in `image` (`image@@offset` allowed).
pub fn mcopy(image: &str, src: &Path, dest: &str) {
    tool(
        "mcopy",
        &[
            "-m",
            "-i",
            image,
            src.to_str().unwrap(),
            &format!("::/{dest}"),
        ],
    );
}

/// Parses `list --json` output.
pub fn list_json(cwd: &Path, img: &str, extra: &[&str]) -> (i32, Vec<serde_json::Value>) {
    let mut args = vec!["list", img, "--json"];
    args.extend_from_slice(extra);
    let o = sdrescue(cwd, &args);
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|e| panic!("bad JSON ({e}):\n{}", text(&o)));
    (code(&o), rows)
}

/// The row whose `path` equals `path`.
pub fn row<'a>(rows: &'a [serde_json::Value], path: &str) -> &'a serde_json::Value {
    rows.iter()
        .find(|r| r["path"] == path)
        .unwrap_or_else(|| panic!("{path} not listed; got {rows:#?}"))
}
