# sdrescue

**sdrescue** gets deleted **photos, videos and audio** back from SD cards formatted
with **FAT12, FAT16 or FAT32**, the format most cameras, phones and recorders use.
It's a single small program for Windows (and Linux) with no installer, no adware
and no network access.

## What it does

- **Copies the card into an image file** (`image`), byte for byte. Unreadable sectors
  are skipped and logged, and the image gets a SHA-256 hash.
- **Shows what's on the card** (`info`): the partition table, the filesystem and its
  geometry.
- **Lists deleted files** (`list`) with their original names and folders, and says
  whether each one can still be recovered.
- **Undeletes files** (`undelete`) with their names, folders and dates.
- **Carves files** (`carve`) by recognising their content (JPEG, PNG, GIF, HEIC, TIFF and
  RAW, MP4/MOV, AVI, MTS, WMV, MP3, WAV, M4A, OGG/Opus, FLAC, AMR). This still works
  after the card was reformatted or the folder entries are gone. AVI videos whose
  header was overwritten are rebuilt into playable files.

## What it doesn't do

- It never writes to the card, and it won't save recovered files onto the card.
- It can't bring back data that has been physically overwritten by new photos.
- It can't fix a card that has failed electronically. If the card isn't detected
  at all, see [troubleshooting](docs/troubleshooting.md).
- `list` and `undelete` only understand **FAT**. On **exFAT** cards (most cards of
  64 GB and up) or NTFS, use `carve`.
- It doesn't repair broken videos. Videos missing their index (`moov`) need a tool such
  as [untrunc](https://github.com/anthwlock/untrunc).

## ⚠️ Before you start

1. **Stop using the card now.** Don't take new photos with it and don't copy anything
   onto it. Every new file can overwrite the ones you want back.
2. **Don't let Windows "scan and fix" it.** If Windows offers to *Scan and fix* or to
   *format* the card, click **Cancel** or close the window. Repairing or formatting
   can destroy deleted files.
3. **Never save recovered files to the card.** Save them to your PC's drive (`C:`).
   sdrescue refuses to write onto the card it is reading.
4. **Make an image first.** Copy the whole card to an image file once, then do all the
   other steps on the copy. This puts the least strain on a card that may be failing,
   and you can try again as often as you like.

## Quick start (Windows)

1. **Open an admin terminal.** Click Start, type `terminal`, right-click **Terminal**
   (or *Windows PowerShell* or *Command Prompt*) and choose **Run as administrator**.
   Then go to the folder that holds `sdrescue.exe`:

   ```
   cd C:\Users\<you>\sdrescue
   ```

2. **Find your card:**

   ```
   sdrescue disks
   ```

   Look for the removable drive with the size of your card. Note its drive letter
   (for example `F:`) and the **physical drive** shown next to it (for example
   `\\.\PhysicalDrive2`).

3. **Make the image.** This copies the whole card and can take a while (about
   1 minute per GB):

   ```
   sdrescue image \\.\PhysicalDrive2 card.img
   ```

   Use your own physical drive number. Imaging the physical drive also captures the
   partition table. If that doesn't work, `sdrescue image F: card.img` images only
   the partition. Once this finishes you can take the card out and keep it safe.

4. **See which deleted files are still there:**

   ```
   sdrescue list card.img
   ```

   `good` files will most likely come back intact, `partial` ones may be damaged,
   and `overwritten` ones are probably lost.

5. **Undelete them,** with their names and folders:

   ```
   sdrescue undelete card.img --out recovered
   ```

   Open the `recovered` folder. `recovered\report.csv` lists every file.

6. **Carve for anything else.** This scans the free space of the card for files by
   their content, which finds files whose folder entries are gone (for example after
   a format):

   ```
   sdrescue carve card.img --out carved
   ```

   Carved files get generic names such as `carved\jpg\f000123456.jpg`.

Every command has `--help` with examples, for example `sdrescue carve --help`. The
[usage guide](docs/usage.md) covers every option.

## Documentation

- [Usage](docs/usage.md): every command and option, sample output, what the statuses
  mean and how to read `report.csv`.
- [How it works](docs/how-it-works.md): why deleted files can be recovered, and how
  carving works.
- [Troubleshooting](docs/troubleshooting.md): access denied, card not listed, bad
  sectors, videos that won't play, exFAT, Windows Defender warnings.
- [Building](docs/building.md): building from source on Linux/WSL and cross-compiling
  `sdrescue.exe`.
- [Changelog](CHANGELOG.md)

## Building

sdrescue is written in Rust. On WSL or Linux:

```
cargo build --release --target x86_64-pc-windows-gnu   # sdrescue.exe for Windows
cargo build --release                                  # native Linux binary
cargo test
```

See [docs/building.md](docs/building.md) for the full setup.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option (`MIT OR Apache-2.0`). See [LICENSE](LICENSE).
