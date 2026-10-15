//! `sdrescue info`: partition table and filesystem geometry.

use anyhow::Result;

use super::Outcome;
use crate::cli::GlobalOpts;
use crate::device::open_for_analysis;
use crate::fat::FatVolume;
use crate::fat::bpb::FatType;
use crate::partition::{FsKind, Scheme, locate};
use crate::util::human_size;

/// Runs `info`.
pub fn run(source: &str, g: &GlobalOpts) -> Result<Outcome> {
    let src = open_for_analysis(source, g.force, "info")?;
    let src = src.as_ref();
    println!(
        "Source:            {} ({}, {} bytes)",
        src.describe(),
        human_size(src.size()),
        src.size()
    );
    let layout = locate(src, g.offset)?;
    match &layout.table {
        None => println!("Partition table:   none (the filesystem starts at sector 0)"),
        Some(t) => {
            println!(
                "Partition table:   {}",
                match t.scheme {
                    Scheme::Mbr => "MBR",
                    Scheme::Gpt => "GPT",
                }
            );
            println!(
                "  {:>2}  {:<4}  {:>14}  {:>10}  TYPE",
                "#", "BOOT", "START (BYTES)", "SIZE"
            );
            for (i, p) in t.partitions.iter().enumerate() {
                let name = p
                    .name
                    .as_deref()
                    .map(|n| format!(" \"{n}\""))
                    .unwrap_or_default();
                println!(
                    "  {:>2}  {:<4}  {:>14}  {:>10}  {}{}{}",
                    p.index,
                    if p.bootable { "*" } else { "" },
                    p.start,
                    human_size(p.size),
                    p.type_name(),
                    name,
                    if layout.chosen == Some(i) {
                        "   <- analysed"
                    } else {
                        ""
                    }
                );
            }
        }
    }
    if g.offset.is_some() {
        println!("Offset override:   {} bytes (--offset)", layout.fs_offset);
    }
    println!();
    match &layout.fs {
        FsKind::ExFat | FsKind::Ntfs => {
            println!(
                "Filesystem at offset {}: {}",
                layout.fs_offset,
                layout.fs.name()
            );
            println!();
            println!(
                "`list` and `undelete` do not support {} yet. `carve` still works:",
                layout.fs.name()
            );
            println!("    sdrescue carve {source} --out carved");
            return Ok(Outcome::Done);
        }
        FsKind::Unknown(e) => {
            println!(
                "Filesystem at offset {}: not recognised ({e})",
                layout.fs_offset
            );
            println!();
            println!("If the card was reformatted or damaged, `carve` can still find files:");
            println!("    sdrescue carve {source} --out carved");
            return Ok(Outcome::NothingFound);
        }
        FsKind::Fat(_) => {}
    }
    let vol = FatVolume::open(src, layout.fs_offset)?;
    let b = &vol.bpb;
    let walk_label = vol.walk().volume_label;
    println!("Filesystem at offset {}: {}", layout.fs_offset, b.fat_type);
    println!("  OEM name:            {}", b.oem_name);
    println!(
        "  Volume label:        {}",
        walk_label
            .or_else(|| b.label.clone())
            .unwrap_or_else(|| "(none)".into())
    );
    if let Some(id) = b.volume_id {
        println!(
            "  Volume serial:       {:04X}-{:04X}",
            id >> 16,
            id & 0xFFFF
        );
    }
    println!("  Bytes per sector:    {}", b.bytes_per_sector);
    println!("  Sectors per cluster: {}", b.sectors_per_cluster);
    println!(
        "  Cluster size:        {} ({} bytes)",
        human_size(b.cluster_size()),
        b.cluster_size()
    );
    println!("  Reserved sectors:    {}", b.reserved_sectors);
    println!(
        "  FATs:                {} x {} sectors ({} each), FAT #1 at offset {}",
        b.num_fats,
        b.fat_size_sectors,
        human_size(b.fat_bytes()),
        vol.offset + b.fat_offset(0)
    );
    if b.fat_type == FatType::Fat32 {
        println!(
            "  Root directory:      cluster {} (offset {})",
            b.root_cluster,
            vol.cluster_abs(b.root_cluster).unwrap_or(0)
        );
    } else {
        println!(
            "  Root directory:      fixed, {} entries at offset {} ({} sectors)",
            b.root_entries,
            vol.offset + b.root_dir_offset(),
            b.root_dir_sectors
        );
    }
    let (data_start, data_end) = vol.data_region();
    println!("  Data region:         offset {data_start} to {data_end}");
    println!(
        "  Total sectors:       {} ({})",
        b.total_sectors,
        human_size(b.volume_bytes())
    );
    let free = vol.fat.free_count();
    println!("  Total clusters:      {}", b.cluster_count);
    println!(
        "  Free clusters:       {} ({:.1}%, {})",
        free,
        100.0 * f64::from(free) / f64::from(b.cluster_count.max(1)),
        human_size(u64::from(free) * b.cluster_size())
    );
    println!("  FAT in use:          FAT #{}", vol.fat_copy + 1);
    for w in &vol.warnings {
        println!("  warning: {w}");
    }
    Ok(Outcome::Done)
}
