use std::fs;
use std::path::Path;

use anyhow::Result;
use chrono::{Duration, Utc};
use tempfile::TempDir;
use ultron_core::Paths;
use ultron_core::trash::Trash;

fn setup() -> Result<(TempDir, Paths, Trash)> {
    let temp = tempfile::tempdir()?;
    let paths = Paths::from_root(temp.path().join("home"));
    let trash = Trash::new(&paths);
    Ok((temp, paths, trash))
}

#[test]
fn trash_moves_and_restores() -> Result<()> {
    let (temp, paths, trash) = setup()?;
    let source = temp.path().join("report.txt");
    fs::write(&source, "recover me")?;
    let entry = trash.put(&source, Some("synthetic-chat"))?;
    assert!(!source.exists());
    assert_eq!(
        entry.original_path,
        fs::canonicalize(temp.path())?.join("report.txt")
    );
    assert_eq!(entry.size, 10);
    assert!(!entry.is_dir);
    assert_eq!(entry.chat_id.as_deref(), Some("synthetic-chat"));
    let meta = paths.root.join("trash").join(&entry.id).join("meta.json");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(meta)?)?["id"],
        entry.id
    );
    assert_eq!(trash.list()?, vec![entry.clone()]);
    let restored = trash.restore(&entry.id, None)?;
    assert_eq!(fs::read_to_string(restored)?, "recover me");
    assert!(source.exists());
    assert!(trash.list()?.is_empty());
    Ok(())
}

#[test]
fn restore_never_overwrites() -> Result<()> {
    let (temp, _, trash) = setup()?;
    let source = temp.path().join("report.txt");
    fs::write(&source, "old")?;
    let entry = trash.put(&source, None)?;
    fs::write(&source, "current")?;
    let first = temp.path().join("report (geri yüklendi 1).txt");
    fs::write(&first, "other")?;
    let target = trash.restore(&entry.id, None)?;
    assert_eq!(target.file_name().unwrap(), "report (geri yüklendi 2).txt");
    assert_eq!(fs::read_to_string(target)?, "old");
    assert_eq!(fs::read_to_string(source)?, "current");
    assert_eq!(fs::read_to_string(first)?, "other");
    Ok(())
}

#[test]
fn cross_volume_trash_copies_then_deletes() -> Result<()> {
    let (temp, _, trash) = setup()?;
    let source = temp.path().join("tree");
    fs::create_dir_all(source.join("nested/empty"))?;
    fs::write(source.join("a.bin"), [0, 1, 2, 3])?;
    fs::write(source.join("nested/b.txt"), "copied")?;
    let entry = trash.put_with_forced_copy(&source, None)?;
    assert!(!source.exists());
    assert!(entry.is_dir);
    assert_eq!(entry.size, 10);
    assert_eq!(trash.list()?.len(), 1);
    let restored = trash.restore(&entry.id, None)?;
    assert_eq!(fs::read(restored.join("a.bin"))?, [0, 1, 2, 3]);
    assert_eq!(fs::read_to_string(restored.join("nested/b.txt"))?, "copied");
    assert!(restored.join("nested/empty").is_dir());
    Ok(())
}

#[test]
fn retention_purges_only_inside_trash() -> Result<()> {
    let (temp, paths, trash) = setup()?;
    let source = temp.path().join("old.txt");
    fs::write(&source, "trashed")?;
    let entry = trash.put(&source, None)?;
    // A recreated original must never be touched by retention.
    fs::write(&source, "keep")?;
    let outside = temp.path().join("outside.txt");
    fs::write(&outside, "outside")?;
    let meta_path = paths.root.join("trash").join(&entry.id).join("meta.json");
    let mut meta = serde_json::from_slice::<serde_json::Value>(&fs::read(&meta_path)?)?;
    meta["original_path"] = serde_json::to_value(&outside)?;
    fs::write(meta_path, serde_json::to_vec(&meta)?)?;
    assert_eq!(
        trash.purge_expired(30, entry.deleted_at + Duration::days(29))?,
        0
    );
    assert_eq!(
        trash.purge_expired(30, entry.deleted_at + Duration::days(31))?,
        1
    );
    assert_eq!(trash.purge_expired(30, Utc::now())?, 0);
    assert_eq!(fs::read_to_string(source)?, "keep");
    assert_eq!(fs::read_to_string(outside)?, "outside");
    assert!(trash.list()?.is_empty());
    Ok(())
}

#[test]
fn refuses_dangerous_roots() -> Result<()> {
    let (temp, paths, trash) = setup()?;
    fs::create_dir_all(paths.root.join("trash/nested"))?;
    assert!(trash.put(&paths.root, None).is_err());
    assert!(trash.put(&paths.root.join("trash"), None).is_err());
    assert!(trash.put(&paths.root.join("trash/nested"), None).is_err());
    assert!(trash.put(temp.path(), None).is_err());
    let canonical = fs::canonicalize(temp.path())?;
    let drive_root = canonical.ancestors().last().unwrap();
    assert!(trash.put(drive_root, None).is_err());
    if let Some(profile) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        assert!(trash.put(Path::new(&profile), None).is_err());
    }
    assert!(paths.root.join("trash/nested").is_dir());
    Ok(())
}

#[test]
fn restore_supports_explicit_destination_and_missing_parents() -> Result<()> {
    let (temp, _, trash) = setup()?;
    let source = temp.path().join("original");
    fs::write(&source, "data")?;
    let entry = trash.put(&source, None)?;
    let target = temp.path().join("new/parents/renamed");
    let restored = trash.restore(&entry.id, Some(&target))?;
    assert_eq!(fs::read_to_string(restored)?, "data");
    assert!(target.is_file());
    assert!(!source.exists());
    Ok(())
}

#[test]
fn directory_restore_never_overwrites_and_handles_dotfiles() -> Result<()> {
    let (temp, _, trash) = setup()?;
    let source = temp.path().join("folder");
    fs::create_dir(&source)?;
    fs::write(source.join("child"), "saved")?;
    let entry = trash.put(&source, None)?;
    fs::create_dir(&source)?;
    fs::write(source.join("child"), "present")?;
    let restored = trash.restore(&entry.id, None)?;
    assert_eq!(restored.file_name().unwrap(), "folder (geri yüklendi 1)");
    assert_eq!(fs::read_to_string(source.join("child"))?, "present");
    assert_eq!(fs::read_to_string(restored.join("child"))?, "saved");
    let dotfile = temp.path().join(".config");
    fs::write(&dotfile, "dot")?;
    let entry = trash.put(&dotfile, None)?;
    fs::write(&dotfile, "keep")?;
    let restored = trash.restore(&entry.id, None)?;
    assert_eq!(restored.file_name().unwrap(), ".config (geri yüklendi 1)");
    Ok(())
}

#[test]
fn rejects_traversal_corrupt_metadata_and_restore_into_trash() -> Result<()> {
    let (temp, paths, trash) = setup()?;
    assert!(trash.list()?.is_empty());
    let source = temp.path().join("file");
    fs::write(&source, "saved")?;
    let entry = trash.put(&source, None)?;
    assert!(trash.restore("../outside", None).is_err());
    let target = paths.root.join("trash/escape");
    assert!(trash.restore(&entry.id, Some(&target)).is_err());
    assert!(!target.exists());
    let slot = paths.root.join("trash").join(&entry.id);
    fs::write(slot.join("meta.json"), "{")?;
    assert!(trash.list().is_err());
    assert!(trash.restore(&entry.id, None).is_err());
    assert!(trash.purge_expired(0, Utc::now()).is_err());
    assert_eq!(fs::read_to_string(slot.join("item"))?, "saved");
    Ok(())
}

#[cfg(windows)]
fn link_directory(link: &Path, target: &Path) -> Result<()> {
    let output = std::process::Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "synthetic junction creation failed"
    );
    Ok(())
}

#[cfg(unix)]
fn link_directory(link: &Path, target: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[test]
fn refuses_links_in_sources_trash_and_retention() -> Result<()> {
    let (temp, paths, trash) = setup()?;
    let outside = temp.path().join("outside");
    fs::create_dir(&outside)?;
    fs::write(outside.join("keep"), "safe")?;
    let linked = temp.path().join("linked");
    link_directory(&linked, &outside)?;
    assert!(trash.put(&linked, None).is_err());
    let tree = temp.path().join("tree");
    fs::create_dir(&tree)?;
    link_directory(&tree.join("nested-link"), &outside)?;
    assert!(trash.put_with_forced_copy(&tree, None).is_err());
    assert!(tree.exists());
    let source = temp.path().join("file");
    fs::write(&source, "saved")?;
    let entry = trash.put(&source, None)?;
    let slot = paths.root.join("trash").join(&entry.id);
    link_directory(&slot.join("outside-link"), &outside)?;
    assert!(trash.purge_expired(0, Utc::now()).is_err());
    assert_eq!(fs::read_to_string(outside.join("keep"))?, "safe");
    assert!(slot.join("item").exists());
    Ok(())
}

#[test]
fn refuses_redirected_trash_root() -> Result<()> {
    let (temp, paths, trash) = setup()?;
    fs::create_dir_all(&paths.root)?;
    let outside = temp.path().join("outside");
    fs::create_dir(&outside)?;
    link_directory(&paths.root.join("trash"), &outside)?;
    let source = temp.path().join("keep");
    fs::write(&source, "safe")?;
    assert!(trash.put(&source, None).is_err());
    assert!(trash.list().is_err());
    assert!(trash.purge_expired(0, Utc::now()).is_err());
    assert_eq!(fs::read_to_string(source)?, "safe");
    assert_eq!(fs::read_dir(outside)?.count(), 0);
    Ok(())
}

#[cfg(windows)]
#[test]
fn failed_move_preserves_source_and_does_not_poison_listing() -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    let (temp, _, trash) = setup()?;
    let source = temp.path().join("locked");
    fs::write(&source, "preserved")?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&source)?;
    assert!(trash.put(&source, None).is_err());
    assert!(source.exists());
    assert!(trash.list()?.is_empty());
    drop(lock);
    assert_eq!(fs::read_to_string(&source)?, "preserved");
    let entry = trash.put(&source, None)?;
    assert_eq!(trash.list()?, vec![entry]);
    Ok(())
}
