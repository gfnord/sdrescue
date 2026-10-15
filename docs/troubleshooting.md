# Troubleshooting

## "Access denied"

```
error: access denied opening \\.\PhysicalDrive2.
Reading a raw card needs administrator rights. ...
```

Reading a whole card (`disks`, `image`) needs administrator rights on Windows. Close
the terminal, then click Start, type `terminal`, right-click **Terminal** (or
*PowerShell* / *Command Prompt*) and choose **Run as administrator**. Go back to your
folder with `cd` and run the command again.

`info`, `list`, `undelete` and `carve` on an **image file** don't need admin rights.

On Linux, run `sudo sdrescue image /dev/sdX card.img`.

## The card doesn't appear in `disks`

- Run `disks` in an **admin** terminal. Without admin rights some details (such as
  sizes) may show `?`.
- Take the card out and put it back, or try another USB port or card reader. Cheap
  readers often fail with large SDXC cards.
- If the card shows up as a drive letter but with `(no media)`, the reader sees no
  card. Push it in firmly, or try another reader.
- Open **Disk Management** (right-click Start → *Disk Management*). If the card isn't
  there either, Windows doesn't see it at all. Software can't help then, and a data
  recovery lab may.
- **Don't** initialise or format the disk when Disk Management offers to. Click
  **Cancel**.
- In WSL, USB card readers are not visible to Linux by default. Run the Windows
  `sdrescue.exe` from a Windows terminal instead.

## Reader errors and bad sectors

While imaging, sdrescue reads 4 MiB at a time. If a read fails, it retries that chunk
one sector at a time, fills the sectors that still fail with zeros, and keeps going:

```
warning: unreadable sector 123904 (filled with zeros)
...
Warning:     8 unreadable sector(s) (4.0 KB) were filled with zeros; see card.img.log
```

- A few bad sectors usually damage only one or two files. The rest recover fine.
- If errors scroll by continuously, the reader or the card is failing. Stop with
  **Ctrl+C**, try another reader or USB port, then continue where it stopped:

  ```
  sdrescue image \\.\PhysicalDrive2 card.img --resume
  ```

- If the card disconnects during imaging, reconnect it, check with `sdrescue disks`
  that it has the same physical drive number, and use `--resume`.
- The log `card.img.log` lists every bad range. Files with clusters in those ranges
  will have zeros (grey blocks in photos, glitches in videos).

## Recovered videos won't play

Most camera videos (MP4/MOV) keep their index, the **`moov` box**, at the end of the
file. If the camera was switched off while recording, the card filled up, or the end
of the file was overwritten, `moov` is missing and players refuse the file even
though the video data is there.

`carve` marks these files in `report.csv`:

```
note: no moov box: the video index is missing; repair it with untrunc and a reference video from the same camera
```

To repair one:

1. Get [untrunc](https://github.com/anthwlock/untrunc) (there are Windows builds).
2. Find a **working** video recorded by the **same camera with the same settings**
   (resolution, frame rate). It serves as the reference.
3. Run `untrunc reference.mp4 broken.mp4`. It writes `broken.mp4_fixed.mp4`.

Other causes:

- `truncated = yes` in the carve report means the file hit the size cap or the end of
  the image. Raise the cap with `--max-video 16G` if your videos are bigger than 4 GB.
- The file was fragmented (see below).
- MTS (AVCHD) clips play in VLC. Some players need the whole AVCHD folder structure.

## Files come out corrupted

These are typical symptoms:

- Photos with a grey or garbled lower part.
- Photos showing part of another picture.
- Videos that stop or glitch partway.

There are two usual causes:

1. **Overwriting.** New files were written over the clusters of the deleted one.
   `list` shows `overwritten` or `partial`, and `signature` shows `mismatch` when even
   the start was overwritten. That data is gone.
2. **Fragmentation.** The file was not stored in one piece. FAT forgets the cluster
   chain on deletion, so both `undelete` and `carve` assume consecutive clusters, and
   a fragmented file gets the wrong data after its first fragment. `list` may still
   say `good`, because the clusters are free, just not the right ones.
   [How it works](how-it-works.md#the-contiguous-cluster-assumption) explains when
   this happens.

Other things to try:

- If `undelete` gave a corrupted file, check whether `carve` found the same file
  (compare sizes, or open both). Carving finds the true end of JPEGs.
- Photos often still have a usable **EXIF thumbnail** even when the main image is
  damaged. Tools such as ExifTool can extract it.
- `report.csv`: files that are `good` with signature `ok` are the best candidates.

## exFAT cards

Cards of **64 GB and larger** (SDXC) usually come formatted as **exFAT**. `info` says
so:

```
Filesystem at offset 4194304: exFAT

`list` and `undelete` do not support exFAT yet. `carve` still works:
    sdrescue carve card.img --out carved
```

Use `carve`. It doesn't need the filesystem. On exFAT the free-space map can't be read
yet, so `carve` scans the whole image and may also return files that were never
deleted. Recovered files get generic names (`f000123456.jpg`). The photos' EXIF data
still holds the date taken.

## Windows Defender or SmartScreen warns about the exe

`sdrescue.exe` isn't code-signed (a certificate costs money), and it reads raw disks,
which recovery tools do but most programs don't. So Windows may show *"Windows protected
your PC"*, or Defender may scan it for a while.

- SmartScreen: click **More info** → **Run anyway**.
- If Defender quarantines it, you can restore it under *Windows Security → Virus &
  threat protection → Protection history*. Only do this if you built it yourself or
  got it from a source you trust.
- You can always build it yourself from source (see [building](building.md)) and
  compare its SHA-256 (`certutil -hashfile sdrescue.exe SHA256`) with the one
  published with the release.
