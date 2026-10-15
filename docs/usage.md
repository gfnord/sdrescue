# Using sdrescue

```
sdrescue [--offset <BYTES>] [--force] [-v] <COMMAND> ...
```

The usual order is `disks` → `image` → `info` → `list` → `undelete` → `carve`.
Everything after `image` works on the image file, not on the card.

On Windows, `disks` and `image` need a terminal opened with **Run as administrator**.
The other commands don't, because they only read the image file.

## Global options

| Option | Meaning |
|---|---|
| `--offset <BYTES>` | Where the FAT filesystem starts inside the image, in bytes. Overrides partition-table detection. Accepts `1048576`, `1M`, `2048s` (sectors of 512 bytes) and `0x100000`. |
| `--force` | Allows `info`, `list`, `undelete` and `carve` to run directly on a device. Not recommended (see below). |
| `-v`, `--verbose` | More detail: exact sizes, every file written, skipped entries, candidate counts. |
| `-h`, `--help` | Help for sdrescue or for one command (`sdrescue list --help`). |
| `-V`, `--version` | Prints the version. |

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Error (bad arguments, unreadable file, refused for safety, ...) |
| 2 | Finished, but nothing was found or recovered |

### Devices vs. image files

These names are treated as **devices**:

- Windows: `F:`, `F:\`, `\\.\F:`, `\\.\PhysicalDrive2`, and any other `\\.\...` or `\\?\Volume{...}` path.
- Linux: anything under `/dev/`, or any block or character device node.

All other names are image files.

`info`, `list`, `undelete` and `carve` **refuse to run on a device** unless you add
`--force`. The error shows the `sdrescue image ...` command to run instead:

```
> sdrescue list F:
error: 'F:' is a device (a real card or disk), not an image file.

Working directly on the card is risky: every extra read stresses a card that may be
failing, and any mistake (or Windows deciding to "repair" it) can destroy the files
you want back. Make a byte-for-byte image first and work on the copy instead:

    sdrescue image \\.\F: card.img
    sdrescue list card.img
...
If you really want to run directly on the device, add --force.
```

sdrescue also **refuses to write output onto the source**: the image file, recovered
files and reports can't go on the card being read. On Windows it compares drive
letters, volume serial numbers and physical disk numbers. On Linux it compares device
IDs, including partitions of the same disk. It also **never overwrites a file**. If a
name is already taken, it adds a number, as in `IMG_0001 (2).JPG`.

---

## `sdrescue disks`

Lists drives so you can find your card.

- **Windows:** each volume with its drive letter, label, filesystem, size, type
  (removable or fixed) and the physical drive behind it. Then each physical drive
  with its size, bus (USB, SD, ...), removable flag and model.
- **Linux:** the block devices from `/sys/block` with their size, removable flag,
  model, partitions and mount points. `-v` also shows empty devices and RAM disks.

```
> sdrescue disks
Volumes:
  DRIVE  LABEL            FS           SIZE TYPE       PHYSICAL DRIVE
  C:     Windows          NTFS       476 GB fixed      \\.\PhysicalDrive0
  F:     CANON            FAT32       15 GB removable  \\.\PhysicalDrive2

Physical drives:
  DEVICE                     SIZE BUS      REMOVABLE  MODEL                        VOLUMES
  \\.\PhysicalDrive0       477 GB NVMe     no         Samsung SSD 970 EVO          C:
  \\.\PhysicalDrive2        15 GB USB      yes        Generic STORAGE DEVICE       F:
```

(This sample is illustrative; the columns are real, the values depend on your PC.)

## `sdrescue image <SOURCE> <OUTPUT.img>`

Copies the card, byte for byte, into `OUTPUT.img`. This is the only command meant to
read a device, so it doesn't need `--force`.

- `SOURCE` on Windows: `\\.\PhysicalDriveN` (the whole card, **recommended**, because it
  also captures the partition table and anything outside the partition), or `F:` /
  `\\.\F:` (just that volume). On Linux: `/dev/sdX` or `/dev/mmcblkN`, run with `sudo`.
- `--resume` continues an interrupted image (for example after you pressed Ctrl+C or
  the reader disconnected). The partial image is kept up to its last whole sector, the
  existing part is re-hashed and the copy continues from there.

How it works:

- The device is opened read-only.
- It is read in 4 MiB chunks, always at sector-aligned offsets.
- If a chunk fails, it is read again one sector at a time. Sectors that still fail
  are written as zeros and recorded.
- Before starting, it checks that the destination drive has enough free space.

Sample run (a 16 GB card):

```
> sdrescue image \\.\PhysicalDrive2 card.img
Source:      \\.\PhysicalDrive2 (15 GB, 15931539456 bytes, 512-byte sectors)
Image:       card.img
Log:         card.img.log
imaging    [=================>                  ] 7.2 GiB/14.8 GiB 38.1 MiB/s ETA 3m
...
Done:        15 GB written, SHA-256 6c6d1cc1613976f89c902cbc4cb14dffa2ccd9f487bbe8a15ac1d87b99cf264f
Warning:     8 unreadable sector(s) (4.0 KB) were filled with zeros; see card.img.log

Next steps (on the image, not the card):
    sdrescue info card.img
    sdrescue list card.img
```

The sidecar log `card.img.log` looks like this (sample values; this card had one
bad 4 KB block):

```
sdrescue 0.1.0 image log
source: \\.\PhysicalDrive2
output: card.img
size: 15931539456 bytes (15 GB)
sector size: 512 bytes
started: 2026-10-15 16:37:17 UTC
finished: 2026-10-15 16:44:02 UTC
status: complete
sha256: 6c6d1cc1613976f89c902cbc4cb14dffa2ccd9f487bbe8a15ac1d87b99cf264f
bad sectors: 8
bad range: sectors 123904-123911 (8 sectors, bytes 63438848-63442943)
```

While imaging is running, `status:` says `in progress` and bad ranges are added to the
log as they are found.

## `sdrescue info <SOURCE>`

Shows the partition table (MBR, or GPT if there is one), which partition is analysed,
and the filesystem geometry.

```
> sdrescue info disk.img
Source:            disk.img (17 MB, 17825792 bytes)
Partition table:   MBR
   #  BOOT   START (BYTES)        SIZE  TYPE
   1  *            1048576       16 MB  0x0E FAT16 (LBA)   <- analysed

Filesystem at offset 1048576: FAT16
  OEM name:            mkfs.fat
  Volume label:        (none)
  Volume serial:       095E-715C
  Bytes per sector:    512
  Sectors per cluster: 4
  Cluster size:        2.0 KB (2048 bytes)
  Reserved sectors:    4
  FATs:                2 x 32 sectors (16 KB each), FAT #1 at offset 1050624
  Root directory:      fixed, 512 entries at offset 1083392 (32 sectors)
  Data region:         offset 1099776 to 17825792
  Total sectors:       32768 (16 MB)
  Total clusters:      8167
  Free clusters:       8167 (100.0%, 16 MB)
  FAT in use:          FAT #1
```

To pick the filesystem, sdrescue tries these in order:

1. A FAT boot sector in sector 0 (a card with no partition table).
2. The partition-table entry that holds FAT (MBR types `0x01 0x04 0x06 0x0B 0x0C 0x0E`,
   or a GPT basic-data partition).
3. The first non-empty partition.

Use `--offset` to choose a different start. If the filesystem is **exFAT or NTFS**,
`info` says so and suggests `carve`.

## `sdrescue list <SOURCE>`

Walks every directory, including deleted ones, and shows the deleted files:

```
> sdrescue list card.img
PATH                                            SIZE  DATE                 START CLUSTER  STATUS  SIGNATURE
/DCIM/100CANON/Holiday at the beach.jpg        31 KB  2024-06-01 05:34:56              7  good    ok
/DCIM/100CANON/_MG_0002.JPG                    13 KB  2024-06-01 05:34:56             69  good    ok
/Old Stuff/Songs/A song with a long name.mp3  8.3 KB  2024-06-01 05:34:56            152  good    ok
/Old Stuff/_lip.mp4                           4.6 KB  2024-06-01 05:34:56            142  good    ok
/Sound Recording.wav                           23 KB  2024-06-01 05:34:56             95  good    ok

5 deleted file(s): 5 good, 0 partial, 0 overwritten
```

| Option | Meaning |
|---|---|
| `--all` | Also show existing (not deleted) files; their status is `existing`. |
| `--type photo,video,audio` | Only these categories (by extension, case-insensitive). |
| `--json` | Machine-readable output (an array of objects, see below). |
| `-v` | Exact byte sizes, and marks names whose first letter is unknown. |

### Columns

- **path**: the full reconstructed path. Long names are rebuilt from the deleted
  long-file-name entries. When a file has no long name, the first letter of its 8.3
  name is lost on deletion and shown as `_` (for example `_MG_0002.JPG`). When a
  long name exists, the lost letter is recovered exactly from its checksum.
- **size**: file size from the directory entry.
- **date**: last-modified date and time as stored on the card (camera local time).
- **start cluster**: first cluster of the data.
- **status**: see below.
- **signature**: `ok` if the first bytes match the expected magic number for the
  extension (JPEG `FF D8 FF`, PNG, `ftyp` at offset 4, `RIFF`, `ID3`/MPEG frame sync,
  `fLaC`, `OggS`, ...). `mismatch` if they don't, which usually means the data was
  overwritten. `n/a` if there is no known signature for the extension.

### What each status means

| Status | Meaning | What to expect |
|---|---|---|
| `good` | Every cluster the file needs is still free. | Usually recovers intact (unless the file was fragmented). |
| `partial` | The first cluster is free, but some later ones are used by other files. | The start of the file is fine; the rest may be garbage. Photos may show a grey or corrupted lower part. |
| `overwritten` | The first cluster now belongs to another file (or the entry has no valid start). | Almost certainly lost. Try `carve`. |
| `existing` | Not deleted (only with `--all`). | Nothing to recover. |

### JSON output

`--json` prints one object per file:

```json
{
  "path": "/DCIM/100CANON/Holiday at the beach.jpg",
  "name": "Holiday at the beach.jpg",
  "short_name": "HOLIDA~1.JPG",
  "is_dir": false,
  "deleted": true,
  "size": 31440,
  "start_cluster": 7,
  "long_name": true,
  "first_char_guessed": false,
  "modified": "2024-06-01 05:34:56",
  "status": "good",
  "signature": "ok",
  "category": "photo"
}
```

## `sdrescue undelete <SOURCE> --out <DIR>`

Recovers the deleted files that `list` shows into `DIR`. Original folders and names
are kept, and each file gets its modified time from the FAT timestamp.

| Option | Meaning |
|---|---|
| `--out <DIR>` | Output folder (created if needed). It must not be on the card. |
| `--type photo,video,audio` | Only these categories. |
| `--include-overwritten` | Also write `overwritten` files (normally skipped, since they're almost always garbage). |

```
> sdrescue undelete card.img --out recovered
Recovered 5 file(s) into recovered
Report: recovered\report.csv
```

### `report.csv` (undelete)

```
original_path,recovered_path,size,status,signature
/DCIM/100CANON/Holiday at the beach.jpg,DCIM/100CANON/Holiday at the beach.jpg,31440,good,ok
/DCIM/100CANON/_MG_0002.JPG,DCIM/100CANON/_MG_0002.JPG,13200,good,ok
```

| Column | Meaning |
|---|---|
| `original_path` | Path on the card. |
| `recovered_path` | Where the file was written, relative to `--out`. It may carry a ` (2)` suffix if the name was taken. |
| `size` | Size in bytes from the directory entry. |
| `status` | `good`, `partial` or `overwritten` (see above), or `failed` if the file couldn't be written. |
| `signature` | `ok`, `mismatch` or `n/a`. |

Sort by `status` and `signature` in Excel: `good` + `ok` files are the best bets.
If `report.csv` already exists, the new report is `report (2).csv`.

## `sdrescue carve <SOURCE> --out <DIR>`

Finds files by their content, without using the directory entries. Use it after a
format, when the filesystem is damaged, on exFAT cards, or to find files that `list`
doesn't show.

| Option | Meaning |
|---|---|
| `--out <DIR>` | Output folder. Files are saved as `<DIR>/<ext>/f<byte offset>.<ext>`. |
| `--type photo,video,audio` | Only these categories. |
| `--all-space` | Scan the whole image. By default only **free space** (unallocated clusters) is scanned when the FAT can be read, so files that still exist aren't recovered again. |
| `--max-photo <SIZE>` | Largest photo to carve (default `64M`). |
| `--max-video <SIZE>` | Largest video to carve (default `4G`). |
| `--max-audio <SIZE>` | Largest audio file to carve (default `1G`). |

```
> sdrescue carve card.img --out carved
Scanning 39 MB of 39 MB for file headers (free space only; use --all-space for everything)
Carved 8 file(s) (150 KB) into carved: 1 gif, 2 jpg, 1 mp3, 1 mp4, 1 ogg, 1 png, 1 wav
Report: carved\report.csv
```

Supported formats:

| Format | Extensions | End found by |
|---|---|---|
| JPEG | jpg | marker segments, then EOI after the scan data (EXIF thumbnails are skipped) |
| PNG | png | chunks until `IEND` |
| GIF | gif | blocks until the `;` trailer |
| ISO-BMFF | mp4, mov, m4v, 3gp, m4a, heic, cr3 | top-level boxes |
| RIFF | avi, wav, webp | size in the header (AVI: plus `AVIX` extension chunks) |
| MP3 | mp3 | ID3v2 tag + MPEG frames + ID3v1 tag |
| Ogg | ogg, opus | Ogg pages |
| ASF | wmv, wma | top-level objects |
| MPEG-TS (AVCHD) | mts | 192-byte packet sync |
| FLAC | flac | next header (no reliable end) |
| AMR | amr | next header (no reliable end) |
| TIFF / RAW | tif, cr2 | next header (no reliable end) |

### `report.csv` (carve)

```
offset,length,type,end,truncated,file,note
648192,31440,jpg,structure,no,jpg/f000648192.jpg,
693248,58194,png,structure,no,png/f000693248.png,
```

| Column | Meaning |
|---|---|
| `offset` | Byte offset of the file in the image (also in the file name). |
| `length` | Bytes written. |
| `type` | Detected type (extension). |
| `end` | How the end was found. `structure` means the format's own structure was walked to its end, which is reliable. `next-header` means the format has no reliable end, so the file was cut at the next file header found (or at the size limit). It may have extra bytes at the end, which players and viewers normally ignore. |
| `truncated` | `yes` if the file was cut at the size limit or the end of the image before its real end, so it's probably incomplete. |
| `file` | Path relative to `--out`. |
| `note` | Extra information. For example, `no moov box: ...` means a video is missing its index and needs repair (see [troubleshooting](troubleshooting.md)). |

Carving assumes each file is stored in one piece (unfragmented). That is usually the
case for photos on a camera card, but not always for long videos. See
[how it works](how-it-works.md).
