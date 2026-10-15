# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project follows
[Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-10-15

First release.

### Added

- `disks`: lists volumes and physical drives on Windows (letter, label, filesystem,
  size, removable, physical drive number, bus, model) and block devices on Linux.
- `image`:
  - byte-for-byte copy of a volume or physical disk, read-only, in 4 MiB
    sector-aligned reads
  - per-sector retry, with unreadable sectors zero-filled and logged
  - SHA-256 and a `<OUTPUT>.log` sidecar
  - free-space check, progress bar with speed and ETA, and `--resume`
- `info`: MBR/GPT partition table and FAT geometry; exFAT and NTFS are detected and
  explained.
- `list`:
  - deleted files from FAT12/16/32 directories, with long-name reconstruction,
    first-character recovery from the LFN checksum, and deleted directories
  - `good`/`partial`/`overwritten` status and magic-number signature check
  - `--all`, `--type` and `--json`
- `undelete`: contiguous recovery keeping folders, names and modified times;
  `--type`, `--include-overwritten` and `report.csv`.
- `carve`:
  - two-pass signature carving at 512-byte-aligned offsets, free space only by
    default (`--all-space` scans everything)
  - per-category size caps
  - formats: JPEG, PNG, GIF, ISO-BMFF (MP4/MOV/M4V/3GP/M4A/HEIC/CR3), RIFF
    (AVI/WAV/WEBP), MP3, Ogg/Opus, ASF (WMV/WMA), MPEG-TS (MTS/M2TS), and FLAC, AMR
    and TIFF/CR2 (ending at the next header)
  - `report.csv` with end method and truncation
- Safety:
  - analysis commands refuse devices without `--force`
  - output on the source device or volume is refused
  - existing files are never overwritten
- Exit codes: 0 success, 1 error, 2 nothing found.
