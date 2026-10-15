//! The "never work on the card" rules, exercised with fake device names.
//! None of these paths are opened: the checks happen before any device access.

mod common;

use common::*;

#[test]
fn analysis_commands_refuse_devices_without_force() {
    let w = workdir("refuse");
    let cases: [&[&str]; 5] = [
        &["list", "/dev/sdz"],
        &["info", "/dev/mmcblk9"],
        &["carve", r"\\.\PhysicalDrive7", "--out", "out"],
        &["undelete", "F:", "--out", "out"],
        &["list", r"\\.\F:"],
    ];
    for args in cases {
        let o = sdrescue(&w, args);
        let t = text(&o);
        assert_eq!(code(&o), 1, "{args:?}: {t}");
        assert!(t.contains("--force"), "{args:?}: {t}");
        assert!(t.contains("sdrescue image "), "{args:?}: {t}");
    }
    let t = text(&sdrescue(&w, &["list", "F:"]));
    assert!(t.contains(r"sdrescue image \\.\F: card.img"), "{t}");
    assert!(!w.join("out").exists());
}

#[test]
fn refuses_to_write_output_onto_the_source() {
    let w = workdir("onsource");
    let cases: [&[&str]; 5] = [
        &["image", "F:", r"F:\card.img"],
        &["image", r"\\.\f:", "f:/backup/card.img"],
        &["image", "/dev/sdz", "/dev/sdz"],
        &["undelete", "F:", "--force", "--out", r"F:\recovered"],
        &["carve", r"\\.\G:", "--force", "--out", r"\\?\G:\carved"],
    ];
    for args in cases {
        let o = sdrescue(&w, args);
        let t = text(&o);
        assert_eq!(code(&o), 1, "{args:?}: {t}");
        assert!(t.contains("refusing to write"), "{args:?}: {t}");
    }
    // Nothing was created (on Linux these would be relative file names).
    assert_eq!(std::fs::read_dir(&w).unwrap().count(), 0);
}

#[test]
fn usage_errors_exit_with_1_and_help_with_0() {
    let w = workdir("usage");
    assert_eq!(code(&sdrescue(&w, &["list"])), 1);
    assert_eq!(code(&sdrescue(&w, &["frobnicate"])), 1);
    let o = sdrescue(&w, &["carve", "--help"]);
    assert_eq!(code(&o), 0);
    assert!(text(&o).contains("Examples"));
}

#[test]
fn image_file_that_is_not_fat_gives_a_clear_error() {
    let w = workdir("notfat");
    std::fs::write(w.join("junk.img"), vec![0x5Au8; 64 * 1024]).unwrap();
    let o = sdrescue(&w, &["list", "junk.img"]);
    let t = text(&o);
    assert_eq!(code(&o), 1, "{t}");
    assert!(t.contains("no FAT filesystem found"), "{t}");
    assert!(t.contains("carve"), "{t}");
}
