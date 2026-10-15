# How sdrescue works

## How FAT stores files

A FAT card has four parts:

1. The **boot sector**, with the geometry: sector size, cluster size, where
   everything is.
2. The **File Allocation Table (FAT)**, with one entry per cluster (a cluster is the
   unit of storage, typically 32 KB on an SD card). Each entry is either 0 (free), the
   number of the next cluster of the same file, or an end-of-file marker. A file's
   clusters form a *chain*.
3. **Directories**, lists of 32-byte entries. Each entry holds an 8.3 short name
   (`IMG_0001.JPG`), attributes, the modified date and time, the size and the **first
   cluster**. Long names (`Holiday at the beach.jpg`) are stored in extra *LFN* entries
   just before the short entry.
4. The **data area**, where the clusters live.

## What happens when a file is deleted

Deleting a file on FAT does very little:

- The first byte of its directory entry (and of its long-name entries) is replaced by
  `0xE5`, which marks it as deleted.
- Its FAT chain is set to 0, so its clusters become "free".

**The data itself is not erased.** The directory entry still holds the size, the date
and the first cluster, and the contents stay in the data area until new files reuse
those clusters. That's why you must stop using the card: every new photo can land on
the clusters of a deleted one.

## How `list` and `undelete` find deleted files

1. **Locate the filesystem.** sdrescue checks whether sector 0 is a FAT boot sector
   or a partition table (MBR, or GPT behind a protective MBR), and picks the FAT
   partition.
2. **Validate the boot sector.** sdrescue checks every BPB field. The FAT type comes
   from the number of clusters, using Microsoft's rule: fewer than 4085 is FAT12,
   fewer than 65525 is FAT16, otherwise FAT32.
3. **Load the FAT** into memory. If FAT #1 looks damaged (a wrong media byte in entry 0,
   or links pointing outside the volume), FAT #2 is used instead.
4. **Walk the directories,** starting from the root directory (a fixed area on
   FAT12/16, a cluster chain on FAT32). Existing subdirectories are followed through
   their FAT chains, and a visited set prevents loops.
5. **Deleted entries.** An entry starting with `0xE5` is deleted, and the first letter
   of its short name is lost:
   - If long-name entries come right before it, they are put back in order by their
     position (their sequence numbers were overwritten too) and decoded. The long
     name has an 8-bit checksum of the short name. sdrescue tries every valid first
     character until the checksum matches, which gives back the exact short name.
   - Without a long name, the lost letter is shown as `_` (`_MG_0001.JPG`).
6. **Deleted folders.** Their FAT chains are gone, so sdrescue reads their clusters
   one after another from the first cluster. The first cluster must start with the
   `.` and `..` entries, and reading stops as soon as the content no longer looks like
   directory entries or reaches a cluster that is in use. Files inside are listed
   with their full paths.

### The contiguous-cluster assumption

Once a file is deleted, the FAT no longer says which clusters it used. sdrescue
**assumes the file was stored in consecutive clusters**, starting at its first cluster
and running for `size` bytes. The status is worked out from the current FAT:

- `good`: all of those clusters are free.
- `partial`: the first is free but some later ones are in use by other files.
- `overwritten`: the first cluster belongs to another file.

The assumption holds for most camera cards. Cameras write one file at a time onto a
mostly empty card, so each file ends up in one piece. **It fails when a file was
fragmented**, which happens when:

- the card was nearly full and files were deleted and written many times (the free
  space gets scattered),
- two files were written at the same time (some cameras write a video and its
  thumbnail or sidecar file together),
- a long video grew past a gap of free clusters.

A fragmented file recovers correctly up to its first gap, then holds data from other
files. Photos show a grey, shifted or garbled lower part, and videos stop playing
partway through. sdrescue can't detect this, so the status still says `good`. Only
the content tells.

## How carving works

Carving ignores the filesystem and looks at the content itself. Almost every media
format starts with a recognisable *header* (JPEG `FF D8 FF`, PNG `89 50 4E 47`,
MP4 `ftyp` at offset 4, ...).

**Pass 1: find headers.** The image is read in 8 MiB chunks. Files on FAT always start
at a cluster boundary, so only **512-byte-aligned offsets** are checked. At each one, a
multi-pattern matcher (Aho-Corasick) tests all the format headers at once. Each hit is
confirmed by a quick check of the first 4 KB. Checking only aligned offsets is fast
and avoids false positives in the middle of other files.

By default only **free space** is scanned: the area outside the FAT data region and
the unallocated clusters. Existing files are then not "recovered" again. A freshly
formatted card is all free space, so everything is scanned. `--all-space` scans
everything.

**Pass 2: find the ends.** For each header, in order, the format's own structure is
walked to find where the file ends:

- JPEG: marker segments are followed by their lengths. This skips the EXIF thumbnail,
  which is a complete JPEG of its own inside an APP1 segment. After the start-of-scan
  marker, the compressed data is scanned for the `FF D9` end marker, ignoring the
  stuffed `FF 00` and the `FF D0`–`FF D7` restart markers.
- PNG: chunks until `IEND`. GIF: blocks until the `;` trailer.
- MP4/MOV/HEIC: top-level boxes (`ftyp`, `moov`, `mdat`, ...) while their types are
  known, including 64-bit box sizes. A file needs `mdat` or `moov` to be accepted.
  The extension comes from the brand (`qt  ` → mov, `M4A ` → m4a, `heic` → heic, ...).
- AVI/WAV/WEBP: the size in the RIFF header. Large OpenDML AVIs also take the
  `RIFF…AVIX` chunks that follow.
- MP3: the ID3v2 tag, then MPEG audio frames, each one's length calculated from its
  header, then an optional ID3v1 tag. Without an ID3 tag, at least 8 frames in a row
  are required.
- Ogg/Opus: Ogg pages via their segment tables. WMV/WMA: ASF objects. MTS (AVCHD):
  192-byte packets with the `0x47` sync byte at offset 4 of each.
- FLAC, AMR, TIFF and RAW have no reliable end marker, so the file ends at the **next
  header** found (or at the size cap). The report marks these `next-header`.

Any header that starts **inside a file already carved** is skipped, so a JPEG preview
inside a video doesn't become a separate file. Every file is capped at a size limit
(64 MB for photos, 4 GB for videos and 1 GB for audio by default), and at the end of
the image.

Carving relies on the same assumption as undelete: **files are contiguous**. A
fragmented file comes out with the wrong data after the first fragment.

### Videos without `moov`

MP4 and MOV files keep their index (the `moov` box) either at the start or at the
**end**. If a recording was interrupted (battery died, card full), or the end of the
file was overwritten, the `moov` box is missing and players refuse the file. The
video data (`mdat`) is often all there. sdrescue carves such files anyway and adds a
note in `report.csv`. See [troubleshooting](troubleshooting.md#recovered-videos-wont-play)
for repairing them with `untrunc`.

### AVI videos whose header was overwritten

An AVI file is a header (`hdrl`, which gives the codec, frame size, frame rate and
audio format), followed by `movi`, a plain sequence of chunks:

- `00dc` + size + a video frame (with MJPEG, a complete JPEG),
- `01wb` + size + a block of audio.

Cameras write the header first. After a reformat, the first new photos go into the
first clusters, right on top of the old videos' headers, while the frames behind them
survive.

While pass 1 reads the image, it also looks (at any offset, not only at cluster
starts) for a `00dc`/`01wb` chunk header followed by another valid chunk header, and
records one such *hint* per MiB. In pass 2, file headers and hints are handled in
offset order:

1. A hint that isn't inside a file already carved is followed chunk by chunk to the
   end of the run. The walk passes over index (`ix00`) and padding (`JUNK`) chunks
   and stops at the first invalid chunk or the end of the free space.
2. A JPEG frame that happens to start on a 512-byte boundary is part of the stream
   and is not carved as a separate photo.
3. Any other file header found inside the run means newer data overwrote it there,
   so the stream is cut just before it, and that file is carved normally.

The surviving chunks are then written into a new AVI with a fresh header and index.
Without a reference video, the header is built from what the data reveals:

- the frame size, from the first JPEG frame's SOF marker,
- the frame rate, which is assumed (`--avi-fps`),
- the audio, assumed to be 16-bit PCM. Its sample rate is the standard rate that best
  matches the amount of audio per frame at that frame rate.

With `--avi-reference`, the header of a real video from the same camera is copied
instead, with its frame and sample counts updated, and its OpenDML super-index
(which points into the original file) blanked out.

## Why imaging first matters

- **A failing card can get worse with every read.** Imaging reads each sector exactly
  once, in order, which is the gentlest pattern. Recovery tools that read the card
  directly jump around and read the same areas many times.
- **You can retry as often as you like.** All the analysis runs on the image, so a
  mistake or a crash costs nothing, and you can try other tools on the same image.
- **Nothing can write to the card.** sdrescue never opens the card for writing, and it
  refuses to save output onto it. Windows itself can still write to a card: it may
  update the filesystem, create `System Volume Information`, or offer to "repair" it.
  Once the image is made, the card can go in a drawer.
- **Bad sectors are dealt with once.** The `image` command retries failing chunks
  sector by sector, records the unreadable sectors and fills them with zeros, so the
  later steps never have to fight read errors.

## Code layout

| Module | Purpose |
|---|---|
| `src/source.rs` | `Source` trait (read-only random access) and image files |
| `src/device/` | device name detection, `--force` and output-on-source checks; `linux.rs` / `windows.rs` for raw devices, disk listing and free space |
| `src/partition.rs` | MBR/GPT parsing and choosing the filesystem |
| `src/fat/` | boot sector (`bpb.rs`), FAT decoding (`table.rs`), directory entries and LFN (`dir.rs`), directory walk (`mod.rs`) |
| `src/recover.rs` | status, signature check and contiguous copy for undelete |
| `src/carve/` | carving engine (`engine.rs`), cached reader, one module per format behind the `Carver` trait, and `avistream.rs` for rebuilding AVIs whose header was lost |
| `src/imaging.rs` | the imaging loop, bad sectors, SHA-256 log and resume |
| `src/commands/` | the subcommands |

Adding a carver means implementing `Carver` (signatures, `identify`, `carve`) in a new
module and adding it to `all_carvers()` in `src/carve/mod.rs`.
