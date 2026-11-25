// The record is the point of commitment. Before it exists a writing tree
// may be incomplete. Afterwards the record and all writing trees are durable.
use super::*;
use std::fs::{self, File};
use std::io::{ErrorKind, Write};

const RECORD: &str = ".anno-editor/save";

#[derive(Serialize, Deserialize)]
struct Record {
    before: Vec<String>,
    after: Vec<String>,
    manifest: String,
    settings: String,
}

pub(super) fn lock(folder: &Path) -> Result<File> {
    fs::create_dir_all(folder)?;
    // Lock the directory inode so opening an existing mod on Unix needs no
    // write access. Windows file locks require a regular file.
    #[cfg(unix)]
    let lock = File::open(folder)?;
    #[cfg(not(unix))]
    let lock = {
        let dir = folder.join(".anno-editor");
        fs::create_dir_all(&dir)?;
        File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("writer"))?
    };
    lock.try_lock()
        .with_context(|| format!("Locking project {}", folder.display()))?;
    Ok(lock)
}

fn sync_dir(path: &Path) -> Result<()> {
    // Windows can't open a directory as a file to flush it.
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create_new(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub(super) fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("writing");
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() == ErrorKind::NotFound => (),
        r => r?,
    }
    write_file(&tmp, bytes)?;
    fs::rename(tmp, path).with_context(|| format!("Publishing {}", path.display()))?;
    sync_dir(path.parent().context("missing parent folder")?)
}

fn directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) => {
            ensure!(m.is_dir(), "{} is not a project directory", path.display());
            fs::read_dir(path)?.next().transpose()?;
            Ok(true)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn split_path<'a>(path: &'a str, roots: &[&str]) -> Result<(&'a str, &'a str)> {
    let (root, rel) = path.split_once('/').context("file needs a project root")?;
    ensure!(
        roots.contains(&root)
            && rel
                .split('/')
                .all(|p| !p.is_empty() && !matches!(p, "." | "..") && !p.contains(['\\', ':'])),
        "{path} is outside the project roots"
    );
    Ok((root, rel))
}

#[derive(Clone, Copy)]
enum Action {
    Keep,
    DiscardWriting,
    Restore,
    CleanOld,
    Swap,
    Promote,
}

pub(super) fn recover(folder: &Path, roots: &[&str]) -> Result<()> {
    let path = folder.join(RECORD);
    let record: Option<Record> = match fs::read(&path) {
        Ok(b) => Some(serde_json::from_slice(&b).context("Unreadable save record")?),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if let Some(r) = &record {
        for names in [&r.before, &r.after] {
            ensure!(
                names
                    .iter()
                    .enumerate()
                    .all(|(i, n)| roots.contains(&n.as_str()) && !names[..i].contains(n)),
                "unknown or duplicate root in save record"
            );
        }
        ensure!(
            r.before.iter().all(|n| r.after.contains(n)),
            "missing previous root in save record"
        );
        let _: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&r.manifest).context("invalid manifest in save record")?;
        let settings: Settings =
            serde_json::from_str(&r.settings).context("invalid settings in save record")?;
        for island in settings.islands {
            let (root, _) = split_path(&island.path, roots)?;
            ensure!(
                r.after.iter().any(|n| n == root),
                "missing island root in save record"
            );
        }
    }
    // Without a record nothing was committed, so a lone staging tree beside a
    // valid published project is an abandoned save of a new root and goes.
    // Without valid settings it may be the only copy of a first save: kept.
    let published = record.is_none()
        && fs::read(folder.join(SETTINGS))
            .ok()
            .is_some_and(|b| serde_json::from_slice::<Settings>(&b).is_ok());
    let mut actions = Vec::new();
    // Decide for every root before changing any. An ambiguous second root
    // must leave the first root's backup and staging tree available too.
    for &root in roots {
        let live = folder.join(root);
        let old = folder.join(format!(".{root}.old"));
        let writing = folder.join(format!(".{root}.writing"));
        let state = (directory(&live)?, directory(&old)?, directory(&writing)?);
        let action = if let Some(r) = &record {
            let before = r.before.iter().any(|n| n == root);
            let after = r.after.iter().any(|n| n == root);
            match (before, after, state) {
                (false, false, (false, false, false)) => Action::Keep,
                (true, true, (true, false, true)) => Action::Swap,
                (true, true, (false, true, true)) | (false, true, (false, false, true)) => {
                    Action::Promote
                }
                (true, true, (true, true, false)) => Action::CleanOld,
                (_, true, (true, false, false)) => Action::Keep,
                _ => bail!("Cannot recover committed root {root} with live/old/writing {state:?}"),
            }
        } else {
            match state {
                (true, false, true) => Action::DiscardWriting,
                (false, true, _) => Action::Restore,
                (true, true, false) => Action::CleanOld,
                (_, false, false) => Action::Keep,
                (false, false, true) if published => Action::DiscardWriting,
                (false, false, true) => bail!(
                    "Interrupted first save of {root}; kept {}",
                    writing.display()
                ),
                _ => bail!("Ambiguous save of {root}; kept live, old and writing trees"),
            }
        };
        actions.push((live, old, writing, action));
    }
    if record.is_none() {
        // The legacy settings can still describe the old generation. Validate
        // its roots before dropping any backup. A syntax error in settings
        // is handled separately by the startup migration.
        match fs::read(folder.join(SETTINGS)) {
            Ok(bytes) => {
                if let Ok(settings) = serde_json::from_slice::<Settings>(&bytes) {
                    for island in settings.islands {
                        let (r, _) = split_path(&island.path, roots)?;
                        ensure!(
                            folder.join(r).try_exists()?
                                || folder.join(format!(".{r}.old")).try_exists()?,
                            "missing project root {r} for {}",
                            island.path
                        );
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    for (live, old, writing, action) in &actions {
        match action {
            Action::Swap => {
                fs::rename(live, old)?;
                sync_dir(folder)?;
                fs::rename(writing, live)?;
            }
            Action::Promote => fs::rename(writing, live)?,
            Action::Restore => fs::rename(old, live)?,
            _ => continue,
        }
        sync_dir(folder)?;
    }
    if let Some(r) = &record {
        atomic(&folder.join("modinfo.json"), r.manifest.as_bytes())?;
        atomic(&folder.join(SETTINGS), r.settings.as_bytes())?;
    }
    for (_, old, writing, action) in &actions {
        let remove = match action {
            Action::CleanOld | Action::Swap | Action::Promote => old,
            Action::DiscardWriting | Action::Restore => writing,
            Action::Keep => continue,
        };
        match fs::remove_dir_all(remove) {
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            r => r?,
        }
        sync_dir(folder)?;
    }
    if record.is_some() {
        fs::remove_file(&path)?;
        sync_dir(path.parent().unwrap())?;
    }
    Ok(())
}

impl Project {
    pub(super) fn roots(&self) -> Result<&'static [&'static str]> {
        let roots = self.module.project_roots();
        ensure!(!roots.is_empty(), "This game has no projects");
        ensure!(
            roots.iter().enumerate().all(|(i, r)| !r.is_empty()
                && !r.starts_with('.')
                && !r.contains(['/', '\\', ':'])
                && !roots[..i].contains(r)),
            "Invalid project roots"
        );
        Ok(roots)
    }

    pub fn write(&self) -> Result<PathBuf> {
        let roots = self.roots()?;
        let _lock = lock(&self.folder)?;
        recover(&self.folder, roots)?;
        self.write_locked(roots)
    }

    pub(super) fn write_locked(&self, roots: &[&str]) -> Result<PathBuf> {
        // Until the record names them, the staging trees are this call's
        // alone: an error here (a full disk, an unreadable kept file)
        // removes them, so a failed first save doesn't leave a tree that
        // `recover` must keep as the only copy. A crash still leaves them.
        let mut staged = Vec::new();
        let record = match self.stage_trees(roots, &mut staged) {
            Ok(record) => record,
            Err(e) => {
                for tree in &staged {
                    if let Err(e) = fs::remove_dir_all(tree) {
                        eprintln!(
                            "project save: kept {} after a failed save: {e}",
                            tree.display()
                        );
                    }
                }
                return Err(e);
            }
        };
        // Once renamed into place the record may name the trees even if
        // the sync after fails, so from here on they stay for `recover`.
        atomic(&self.folder.join(RECORD), &record)?;
        recover(&self.folder, roots)?;
        Ok(self.folder.clone())
    }

    /// Writes every root into `.<root>.writing`, each pushed to `staged` as
    /// it is made; gives the save record that commits them.
    fn stage_trees(&self, roots: &[&str], staged: &mut Vec<PathBuf>) -> Result<Vec<u8>> {
        let folder = &self.folder;
        let maps = self.module.islands().and_then(|i| i.maps());
        ensure!(
            maps.is_some() || self.maps.is_empty(),
            "This game has no test maps"
        );
        let mut before = Vec::new();
        for &r in roots {
            if directory(&folder.join(r))? {
                before.push(r.to_owned());
            }
        }
        let mut after = before.clone();
        if !after.iter().any(|r| r == roots[0]) {
            after.push(roots[0].to_owned());
        }
        let generated = self.generated();
        for path in self
            .files
            .keys()
            .chain(self.kept.keys())
            .map(String::as_str)
            .chain(generated.iter().map(|(p, _)| p.as_str()))
            .chain(self.islands.iter().map(|i| i.path.as_str()))
        {
            let (root, _) = split_path(path, roots)?;
            if !after.iter().any(|r| r == root) {
                after.push(root.to_owned());
            }
        }
        for r in &after {
            let tree = folder.join(format!(".{r}.writing"));
            fs::create_dir(&tree)?;
            staged.push(tree);
        }
        let target = |path: &str| -> Result<PathBuf> {
            let (r, rel) = split_path(path, roots)?;
            let to = folder.join(format!(".{r}.writing")).join(rel);
            fs::create_dir_all(to.parent().unwrap())?;
            Ok(to)
        };
        for (path, bytes) in &self.files {
            if !generated.iter().any(|(p, _)| p == path) {
                write_file(&target(path)?, bytes)?;
            }
        }
        for kept in self.kept.values() {
            let from = folder.join(&*kept.rel);
            let to = target(&kept.rel)?;
            if fs::hard_link(&from, &to).is_err() {
                fs::copy(&from, &to)?;
                File::open(&to)?.sync_all()?;
            }
        }
        for (path, body) in generated {
            let own = self.files.get(&path).map(Vec::as_slice);
            write_file(&target(&path)?, merge_modops(own, &body)?.as_bytes())?;
        }
        // A synced file still needs durable entries for each directory in its path.
        let mut dirs: Vec<_> = after
            .iter()
            .map(|r| folder.join(format!(".{r}.writing")))
            .collect();
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(&dir)? {
                let e = entry?;
                if e.file_type()?.is_dir() {
                    dirs.push(e.path());
                }
            }
            sync_dir(&dir)?;
        }
        fs::create_dir_all(folder.join(".anno-editor"))?;
        sync_dir(folder)?;
        let settings = Settings {
            name: String::new(),
            islands: self.islands.clone(),
            guid_base: self.guid_base,
            guid_reserved: self.guid_reserved,
            maps: self.maps.clone(),
            layouts: self.layouts.clone(),
            effects: self.effects.clone(),
        };
        let record = Record {
            before,
            after,
            manifest: self.info.encode(self.module),
            settings: serde_json::to_string_pretty(&settings)?,
        };
        Ok(serde_json::to_vec(&record)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::games::fixtures::PartialGame;

    const ROOTS: [&str; 2] = ["data", "addondata"];
    const GAME: &PartialGame = &PartialGame::Islands;

    fn generation(folder: &Path, name: &str) -> Project {
        let mut p = Project::create(folder, name, "isles", GAME).unwrap();
        for (i, root) in ROOTS.iter().enumerate() {
            let path = format!("{root}/{name}.fixture");
            p.stage(&path, name.as_bytes().to_owned());
            p.register_island(IslandEntry {
                path,
                guid: 123 + i as u32,
                region: "Roman".into(),
                size: "Small".into(),
                listing: Listing {
                    in_pool: false,
                    ..Default::default()
                },
            });
        }
        p.write().unwrap();
        p
    }

    // Both the old and new listings name their own island bytes. Only the
    // record decides which manifest and listing belong to the live trees.
    fn interrupted(dir: &Path, commit: bool) -> PathBuf {
        let folder = dir.join("project");
        let next = dir.join("next");
        generation(&folder, "old");
        generation(&next, "new");
        for root in ROOTS {
            fs::rename(next.join(root), folder.join(format!(".{root}.writing"))).unwrap();
        }
        if commit {
            let record = Record {
                before: ROOTS.map(String::from).into(),
                after: ROOTS.map(String::from).into(),
                manifest: fs::read_to_string(next.join("modinfo.json")).unwrap(),
                settings: fs::read_to_string(next.join(SETTINGS)).unwrap(),
            };
            fs::write(folder.join(RECORD), serde_json::to_vec(&record).unwrap()).unwrap();
        }
        folder
    }

    fn check(folder: &Path, name: &str) {
        let p = Project::open(folder, GAME).unwrap();
        assert_eq!(p.info.name, name);
        assert_eq!(p.islands.len(), 2);
        for (i, root) in ROOTS.iter().enumerate() {
            let path = format!("{root}/{name}.fixture");
            assert_eq!(p.islands[i].path, path);
            assert_eq!(&*p.read_file(&path).unwrap(), name.as_bytes());
            assert!(!folder.join(format!(".{root}.old")).exists());
            assert!(!folder.join(format!(".{root}.writing")).exists());
        }
        assert!(!folder.join(RECORD).exists());
    }

    #[test]
    fn open_finishes_a_committed_save_across_both_roots() {
        let dir = tempfile::tempdir().unwrap();
        let folder = interrupted(dir.path(), true);
        fs::rename(folder.join("data"), folder.join(".data.old")).unwrap();
        check(&folder, "new");
    }

    #[test]
    fn open_discards_an_uncommitted_save() {
        let dir = tempfile::tempdir().unwrap();
        let folder = interrupted(dir.path(), false);
        check(&folder, "old");
    }

    #[test]
    fn open_restores_a_root_an_older_crash_left_as_old() {
        let dir = tempfile::tempdir().unwrap();
        let folder = interrupted(dir.path(), false);
        fs::rename(folder.join("data"), folder.join(".data.old")).unwrap();
        // Second trunk crash state: a live tree and an obsolete backup.
        fs::rename(
            folder.join(".addondata.writing"),
            folder.join(".addondata.old"),
        )
        .unwrap();
        check(&folder, "old");
    }

    #[test]
    fn recovery_repeats_safely() {
        for old_left in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let folder = interrupted(dir.path(), true);
            fs::rename(folder.join("data"), folder.join(".data.old")).unwrap();
            fs::rename(folder.join(".data.writing"), folder.join("data")).unwrap();
            if !old_left {
                fs::remove_dir_all(folder.join(".data.old")).unwrap();
            }
            check(&folder, "new");
            check(&folder, "new");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_restore_keeps_the_backup() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let folder = interrupted(dir.path(), false);
        fs::rename(folder.join("data"), folder.join(".data.old")).unwrap();
        let permissions = fs::metadata(&folder).unwrap().permissions();
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o555)).unwrap();
        let result = Project::open(&folder, GAME);
        fs::set_permissions(&folder, permissions).unwrap();
        assert!(result.is_err());
        assert_eq!(
            fs::read(folder.join(".data.old/old.fixture")).unwrap(),
            b"old"
        );
        assert!(folder.join(".data.writing/new.fixture").exists());
        check(&folder, "old");
    }

    // A first save that fails in the editor's own hands (here a file whose
    // path runs through another file) takes back the trees it staged, so
    // the folder isn't left blocked as an interrupted first save.
    #[test]
    fn a_failed_first_save_leaves_the_folder_usable() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("project");
        fs::create_dir_all(&folder).unwrap();
        let mut p = Project::empty(&folder, "first", "first", GAME);
        p.stage("data/a", b"a file".to_vec());
        p.stage("data/a/b", b"under a file".to_vec());
        assert!(p.write().is_err());
        for root in ROOTS {
            assert!(!folder.join(format!(".{root}.writing")).exists());
        }
        assert!(!folder.join(RECORD).exists());
        let mut p = Project::create(&folder, "first", "first", GAME).unwrap();
        p.stage("data/a", b"a file".to_vec());
        p.write().unwrap();
        drop(p);
        let back = Project::open(&folder, GAME).unwrap();
        assert_eq!(&*back.read_file("data/a").unwrap(), b"a file");
    }

    #[test]
    fn a_save_after_recovery_keeps_large_kept_files() {
        let dir = tempfile::tempdir().unwrap();
        let folder = interrupted(dir.path(), false);
        let large = vec![0x76; KEEP_FROM as usize + 3];
        fs::write(folder.join("addondata/Large.dds"), &large).unwrap();
        let mut p = Project::open(&folder, GAME).unwrap();
        assert!(p.kept.contains_key("addondata/large.dds"));
        // A retry on this open project must recover before linking kept files.
        fs::rename(folder.join("addondata"), folder.join(".addondata.old")).unwrap();
        p.info.name = "saved".into();
        p.write().unwrap();
        drop(p);
        let p = Project::open(&folder, GAME).unwrap();
        assert_eq!(p.info.name, "saved");
        assert_eq!(&*p.read_file("addondata/large.dds").unwrap(), &large);
    }

    #[test]
    fn two_roots_save_and_reopen_without_packaging() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("project");
        generation(&folder, "old");
        let mut p = Project::open(&folder, GAME).unwrap();
        assert!(p.prepare_install(dir.path()).is_err());
        p.info.name = "updated".into();
        p.stage("addondata/new.fixture", b"updated".to_vec());
        p.write().unwrap();
        drop(p);
        let back = Project::open(&folder, GAME).unwrap();
        assert_eq!(back.info.name, "updated");
        assert_eq!(back.file_count(), 3);
        assert_eq!(
            &*back.read_file("addondata/new.fixture").unwrap(),
            b"updated"
        );
    }

    // Roots belong to the game, not to its islands: a game with screens and
    // no islands keeps its projects.
    #[test]
    fn a_ui_only_game_creates_saves_and_reopens_a_project() {
        let screens: &PartialGame = &PartialGame::Screens;
        assert!(screens.islands().is_none());
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("project");
        let mut p = Project::create(&folder, "Screens", "screens", screens).unwrap();
        p.stage("data/ui/one.screen", b"one".to_vec());
        p.stage("addondata/ui/two.screen", b"two".to_vec());
        p.write().unwrap();
        drop(p);
        let mut back = Project::open(&folder, screens).unwrap();
        assert_eq!(back.info.name, "Screens");
        assert_eq!(&*back.read_file("data/ui/one.screen").unwrap(), b"one");
        assert_eq!(&*back.read_file("addondata/ui/two.screen").unwrap(), b"two");
        back.stage("data/ui/one.screen", b"edited".to_vec());
        back.write().unwrap();
        let config = dir.path().join("config");
        let loaded = Project::load(&config, screens).unwrap();
        loaded.write().unwrap();
        assert!(config.join("mods/editor/modinfo.json").is_file());
        assert_eq!(
            fs::read(folder.join("data/ui/one.screen")).unwrap(),
            b"edited"
        );
    }

    // A game the registry doesn't know saves under its own root, with its
    // own manifest number and no installer, and reopens with its models: a
    // mesh past `KEEP_FROM` is still read, because its kind names files.
    #[test]
    fn a_skeleton_project_saves_and_reopens_without_packaging() {
        use crate::games::fixtures::SKELETON;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("project");
        let mut p = Project::create(&folder, "Rocks", "rocks", &SKELETON).unwrap();
        let mut big = b"skm 1\nmesh content/meshes/big.rdm\n".to_vec();
        while big.len() <= KEEP_FROM as usize {
            big.extend(b"# padding, as an exporter's comments would be\n");
        }
        p.stage("content/meshes/big.skm", big.clone());
        p.stage("content/meshes/big.rdm", vec![7; KEEP_FROM as usize + 1]);
        p.write().unwrap();
        assert!(p.prepare_install(dir.path()).is_err());
        drop(p);
        assert!(folder.join("content/meshes/big.skm").is_file());
        assert!(!folder.join("data").exists());
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(folder.join("modinfo.json")).unwrap()).unwrap();
        assert_eq!(manifest["Anno"], 99);
        assert!(manifest.get("Difficulty").is_none());

        let mut back = Project::open(&folder, &SKELETON).unwrap();
        assert_eq!(back.info.name, "Rocks");
        assert_eq!(
            &*back.read_file("content/meshes/big.skm").unwrap(),
            &big[..]
        );
        let models = back.models();
        assert_eq!(models.len(), 1);
        assert_eq!(
            models[0].files,
            ["content/meshes/big.rdm", "content/meshes/big.skm"]
        );
        back.info.name = "Rocks again".into();
        back.write().unwrap();
        drop(back);
        assert_eq!(
            Project::open(&folder, &SKELETON).unwrap().info.name,
            "Rocks again"
        );
    }

    #[test]
    fn a_game_without_roots_has_no_projects() {
        let bare: &PartialGame = &PartialGame::Bare;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("project");
        assert!(Project::create(&folder, "Bare", "bare", bare).is_err());
        fs::create_dir(&folder).unwrap();
        assert!(Project::open(&folder, bare).is_err());
        let config = dir.path().join("config");
        let p = Project::load(&config, bare).unwrap();
        assert!(p.write().is_err());
        assert_eq!(fs::read_dir(&folder).unwrap().count(), 0);
        assert!(
            !config.exists(),
            "nothing written for a game without projects"
        );
    }

    #[test]
    fn an_unused_root_stays_absent_and_a_concurrent_writer_cannot_start() {
        let dir = tempfile::tempdir().unwrap();
        let p = Project::create(dir.path(), "Isles", "isles", GAME).unwrap();
        let held = lock(dir.path()).unwrap();
        assert!(p.write().is_err());
        assert!(!dir.path().join(".data.writing").exists());
        drop(held);
        p.write().unwrap();
        assert!(!dir.path().join("addondata").exists());
        assert!(Project::open(dir.path(), GAME).unwrap().islands.is_empty());
    }

    #[test]
    fn ambiguous_or_unreadable_roots_preserve_both_generations() {
        for committed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let folder = interrupted(dir.path(), committed);
            fs::create_dir(folder.join(".addondata.old")).unwrap();
            assert!(Project::open(&folder, GAME).is_err());
            assert!(folder.join(".data.writing/new.fixture").exists());
            assert!(folder.join("data/old.fixture").exists());
            fs::remove_dir(folder.join(".addondata.old")).unwrap();
            fs::write(folder.join(".addondata.old"), "not a directory").unwrap();
            assert!(Project::open(&folder, GAME).is_err());
            assert!(folder.join(".data.writing/new.fixture").exists());
        }
    }

    #[test]
    fn a_recent_project_that_cannot_reopen_falls_back_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("mods/other");
        generation(&broken, "other");
        fs::write(broken.join(SETTINGS), "{").unwrap();
        super::super::remember(dir.path(), &broken);
        let project = Project::load(dir.path(), GAME).unwrap();
        assert_eq!(project.folder(), dir.path().join("mods/editor"));
        let why = project
            .reopen_failed
            .expect("the reason is kept for the editor");
        assert!(why.contains("mods/other"), "{why}");
        // The broken project is left exactly as it was, for the user to look at.
        assert_eq!(fs::read(broken.join(SETTINGS)).unwrap(), b"{");
        assert!(broken.join("data/other.fixture").is_file());
    }

    #[test]
    fn an_abandoned_new_root_beside_a_published_project_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("project");
        let mut p = Project::create(&folder, "one", "isles", GAME).unwrap();
        p.stage("data/one.fixture", b"one".to_vec());
        p.write().unwrap();
        // A later save added its first addondata file and stopped before its
        // record: only the staging tree exists for that root.
        fs::create_dir(folder.join(".addondata.writing")).unwrap();
        fs::write(folder.join(".addondata.writing/new.fixture"), b"new").unwrap();
        recover(&folder, &ROOTS).unwrap();
        assert!(!folder.join(".addondata.writing").exists());
        assert!(!folder.join("addondata").exists());
        assert_eq!(fs::read(folder.join("data/one.fixture")).unwrap(), b"one");
    }

    #[test]
    fn a_missing_listed_root_and_an_interrupted_first_save_stop_startup() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("mods/editor");
        generation(&folder, "old");
        fs::remove_dir_all(folder.join("addondata")).unwrap();
        assert!(Project::load(dir.path(), GAME).is_err());
        assert!(folder.join(SETTINGS).is_file());
        fs::create_dir(folder.join(".addondata.writing")).unwrap();
        fs::write(folder.join(SETTINGS), "{").unwrap();
        assert!(Project::load(dir.path(), GAME).is_err());
        assert_eq!(fs::read(folder.join(SETTINGS)).unwrap(), b"{");
        assert!(folder.join(".addondata.writing").exists());
    }
}
