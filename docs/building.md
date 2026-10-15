# Building sdrescue

sdrescue is plain Rust (edition 2024, Rust 1.88 or newer). The Windows binary is
cross-compiled from WSL or Linux with the MinGW toolchain, so you don't need Visual
Studio.

## 1. Set up WSL (Windows only)

In an admin PowerShell:

```
wsl --install -d Ubuntu
```

Restart if asked, open **Ubuntu** from the Start menu and create your Linux user.

## 2. Install Rust, the Windows target and MinGW

Inside Ubuntu (WSL) or on Linux:

```
sudo apt update
sudo apt install build-essential mingw-w64
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustup target add x86_64-pc-windows-gnu
rustup component add clippy rustfmt
```

Cargo finds the MinGW linker (`x86_64-w64-mingw32-gcc`) automatically.

## 3. Build

```
git clone <repository-url> sdrescue
cd sdrescue

# Native Linux binary: target/release/sdrescue
cargo build --release

# Windows binary: target/x86_64-pc-windows-gnu/release/sdrescue.exe
cargo build --release --target x86_64-pc-windows-gnu
```

The `.exe` is self-contained: no DLLs and no installer. To run it from Windows, copy it
somewhere on your C: drive, for example:

```
mkdir -p /mnt/c/Users/<you>/sdrescue
cp target/x86_64-pc-windows-gnu/release/sdrescue.exe /mnt/c/Users/<you>/sdrescue/
```

## 4. Run the tests

The integration tests build real FAT images with `mkfs.vfat` (dosfstools) and
`mtools`. No loop mounts, no sudo, and no real devices are used.

```
sudo apt install dosfstools mtools
cargo test
```

Without these tools, the image tests fail with a message saying what to install. To
skip them, set `SDRESCUE_SKIP_FAT_TESTS=1`. If you can't use `sudo`, unpack the
packages into a folder and put it on `PATH`:

```
apt download mtools && dpkg -x mtools_*.deb ~/mtools
PATH=~/mtools/usr/bin:$PATH cargo test
```

Quality checks (CI-equivalent):

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings
```

What the tests cover:

- **Unit tests** (`src/**`): BPB parsing and validation, FAT12/16/32 entry decoding,
  chains and loop guards, LFN decoding and first-character recovery from the
  checksum, partition tables, device-name detection and the safety rules, every
  carver's end detection and truncation, an engine test that every carver survives
  garbage, and imaging with simulated bad sectors and resume.
- **`tests/fat_images.rs`**: FAT12/16/32 images with deleted files in the root and in
  subfolders (with long names), a deleted folder tree, an overwritten file, an
  MBR-partitioned disk, a quick reformat that `list` can't see but `carve` recovers
  byte for byte, a deleted AVI whose header was overwritten and is rebuilt,
  free-space-only carving, `image` with its SHA-256 log, and exit codes.
- **`tests/device_safety.rs`**: `--force` refusal and refusal to write onto the source,
  using fake device names (no hardware is ever opened).

The images are built under `target/tmp/` and are not committed.

## 5. Regenerate the fixtures

The small sample media in `tests/fixtures/` (about 150 KB) are committed. To rebuild
them:

```
python3 -m venv /tmp/venv && /tmp/venv/bin/pip install pillow
PYTHON=/tmp/venv/bin/python tests/fixtures/make_fixtures.sh
```

The script makes:

- JPEG (including one with an EXIF thumbnail and one progressive), PNG and GIF with
  Pillow
- WAV with the Python standard library, and an MJPEG + PCM AVI (frames from Pillow)
- MP3, Ogg Vorbis and MP4 with `ffmpeg` if it's available (set `FFMPEG=/path/to/ffmpeg`
  to choose one). Without ffmpeg, it writes minimal but structurally valid files.

The integration tests compare carved and undeleted files byte for byte with these
fixtures. If you regenerate them, run `cargo test` again.
