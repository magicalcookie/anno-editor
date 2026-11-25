// The layered install view: every `maindata/*.rda` mounted in engine order,
// one winning layer per virtual path, a folder tree and search for the asset
// browser. Mount rules: ../PATCHING.md.

use std::collections::{BTreeMap, HashMap};
use std::hash::BuildHasher;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use foldhash::fast::FixedState;

use crate::rda::{Archive, BlockFlags, Entry, Mapped, Table};

/// Index into [`Install::archives`] in mount order.
pub type ArchiveId = u16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FolderId(pub u32);

/// A child row in the browser tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Node {
    Folder(FolderId),
    File(FileId),
}

/// One archive's say about a path. A path's layers are in mount order; the
/// last one wins and is never a tombstone for an indexed file. 32 bytes: an
/// install has half a million (files are under 4 GB, `mount` checks).
#[derive(Debug, Clone, Copy)]
pub struct Layer {
    offset: u64,
    compressed_size: u32,
    pub raw_size: u32,
    memres: u32,
    pub flags: BlockFlags,
    pub archive: ArchiveId,
}

impl Layer {
    /// This archive deletes the path (from earlier archives).
    pub fn is_tombstone(&self) -> bool {
        self.flags.deleted()
    }
}

#[derive(Debug, Clone, Copy)]
struct Folder {
    // The folder's path in `folder_paths`.
    start: u32,
    len: u32,
    parent: u32,
    subfolders: (u32, u32),
    files: (u32, u32),
}

/// A mounted install. Opening reads only archive headers and header tables.
///
/// Memory: each file's name and each folder's path once (117's 431k files
/// held 44 MB as full paths, 10 MB so), one 32-byte [`Layer`] per (path,
/// archive) pair and small per-folder records. The parsed header tables
/// are dropped after indexing.
pub struct Install {
    dir: PathBuf,
    archives: Vec<Archive>,
    archive_names: Vec<String>,
    // Files in sorted order of their normalized paths. File i's path is its
    // folder's path, '/' and its name, names[name_starts[i]..name_starts[i +
    // 1]]: sorted paths share their folder's prefix, so it is kept once, in
    // `folder_paths`.
    names: String,
    name_starts: Vec<u32>,
    file_folder: Vec<u32>,
    folder_paths: String,
    layer_starts: Vec<u32>,
    layers: Vec<Layer>,
    folders: Vec<Folder>,
    folder_children: Vec<FolderId>,
    folder_files: Vec<FileId>,
    // Counted while indexing, for the tests: every entry is visible,
    // shadowed or deleted, none lost.
    #[cfg_attr(not(test), expect(dead_code))]
    shadowed: usize,
    #[cfg_attr(not(test), expect(dead_code))]
    deleted: usize,
    stamp: u64,
    /// A mounted mod folder: normalized virtual path to its loose file.
    /// Beats every archive, as the game's mod overlay does.
    /// Value: index of the mounted folder, loose file.
    overlay: RwLock<BTreeMap<String, (u16, PathBuf)>>,
}

impl std::fmt::Debug for Install {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Install")
            .field("dir", &self.dir)
            .field("archives", &self.archives.len())
            .field("files", &self.file_count())
            .field("folders", &self.folders.len())
            .finish()
    }
}

#[cfg(test)]
impl Install {
    /// For formats' tests on the 117 and 1800 installs: the mount order
    /// their game crates hand [`Install::open`], copied as a
    /// fixture. The order is the games' fact, not the format's.
    pub(crate) const DATA_FIRST: &[&str] = &["data"];
}

impl Install {
    /// Mounts `game_dir/maindata/*.rda` with `families` first, in that
    /// order (see `archive_files`). The order is the game's: its exe builds
    /// the archive names, which the files don't say.
    pub fn open(game_dir: impl AsRef<Path>, families: &[&str]) -> Result<Install> {
        let dir = game_dir.as_ref().to_path_buf();
        let files = Self::archive_files(&dir, families)?;
        Install::mount(dir, files)
    }

    /// `maindata/*.rda` in mount order: each of `families` in turn, an
    /// archive's family being its lowercase name without the trailing
    /// number (`Data3.rda` is `data`), by number within it; then the rest
    /// by name. Files without an RDA header are left out (2070 ships a
    /// text `checksums.rda`).
    fn archive_files(dir: &Path, families: &[&str]) -> Result<Vec<PathBuf>> {
        let maindata = dir.join("maindata");
        let mut files = Vec::new();
        let listing =
            std::fs::read_dir(&maindata).with_context(|| format!("list {}", maindata.display()))?;
        for item in listing {
            let item = item?;
            let path = item.path();
            let is_rda = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("rda"));
            if is_rda && item.file_type()?.is_file() && crate::rda::Archive::sniff(&path) {
                files.push(path);
            }
        }
        ensure!(
            !files.is_empty(),
            "{}: no .rda archives",
            maindata.display()
        );
        ensure!(files.len() <= ArchiveId::MAX as usize, "too many archives");
        files.sort_by_key(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let lower = name.to_ascii_lowercase();
            let stem = lower.strip_suffix(".rda").unwrap_or(&lower);
            let family = stem.trim_end_matches(|c: char| c.is_ascii_digit());
            let number = stem[family.len()..].parse::<u32>().ok();
            match families.iter().position(|f| *f == family) {
                Some(rank) if number.is_some() => (rank, number.unwrap_or(0), name),
                _ => (families.len(), 0, name),
            }
        });
        Ok(files)
    }

    fn mount(dir: PathBuf, files: Vec<PathBuf>) -> Result<Install> {
        Install::mount_timed(dir, files, &mut Phases::default())
    }

    // Each archive's header tables are parsed on their own thread into flat
    // arrays, keys split into an interned folder and a name. Folders are
    // then interned across archives, each folder's entries layered by name
    // on all cores, and one walk of the folder tree puts files in path
    // order. No step compares or sorts full paths.
    fn mount_timed(dir: PathBuf, files: Vec<PathBuf>, phases: &mut Phases) -> Result<Install> {
        use rayon::prelude::*;
        let stamp = stamp(&dir, &files)?;
        let archive_names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        phases.mark("stamp");

        let scanned: Vec<Scanned> = (files.par_iter().enumerate())
            .map(|(a, path)| Scanned::parse(a as ArchiveId, path))
            .collect::<Result<_>>()?;
        phases.mark("parse archives");

        // Global folders, parents before children: root is 0, and a
        // folder's parent always has a smaller id.
        let mut folders = FolderTable::default();
        folders.intern(b"");
        // (global folder, archive, local folder) in mount order.
        let mut sources: Vec<(u32, ArchiveId, u32)> = Vec::new();
        for (a, sc) in scanned.iter().enumerate() {
            for l in 0..sc.folder_count() {
                sources.push((folders.intern(sc.folder(l)), a as ArchiveId, l as u32));
            }
        }
        let n_folders = folders.paths.len();
        // Stable by folder, so each folder's sources stay in mount order.
        let source_starts = counting_sort(&mut sources, n_folders, |s| s.0);
        phases.mark("intern folders");

        let listed: Vec<Listed> = (0..n_folders)
            .into_par_iter()
            .map(|g| Listed::layer(&scanned, &sources[source_starts[g]..source_starts[g + 1]]))
            .collect();
        phases.mark("layer by name");

        let shadowed = listed.iter().map(|l| l.shadowed).sum();
        let deleted = listed.iter().map(|l| l.deleted).sum();
        let mut install = Install {
            dir,
            archives: Vec::new(),
            archive_names,
            names: String::new(),
            name_starts: Vec::new(),
            file_folder: Vec::new(),
            folder_paths: String::new(),
            layer_starts: Vec::new(),
            layers: Vec::new(),
            folders: Vec::new(),
            folder_children: Vec::new(),
            folder_files: Vec::new(),
            shadowed,
            deleted,
            stamp,
            overlay: RwLock::default(),
        };
        install.build_tree(&folders, &listed);
        phases.mark("folder tree");
        drop(folders);
        let (archives, scratch): (Vec<Archive>, Vec<_>) = (scanned.into_iter())
            .map(|s| (s.archive, (s.records, s.by_folder, s.folder_starts)))
            .unzip();
        install.archives = archives;
        // Over 100 MB for 1800 in many blocks: a few ms to free, off the open.
        rayon::spawn(move || drop((scratch, listed)));
        phases.mark("drop scratch");
        Ok(install)
    }

    // Visits folders depth first, each one's files and subfolders merged
    // the way their full paths sort ("b-c/x" before "b/x": `-` < `/`), so
    // files come out in path order and folders numbered in the order their
    // first file is reached. Folders with nothing visible below are left
    // out. The walk only cuts the order into runs of one folder's files;
    // names and layers are copied run by run after it, on all cores.
    fn build_tree(&mut self, table: &FolderTable, listed: &[Listed]) {
        use rayon::prelude::*;
        let n = listed.len();
        // Visible files below each folder; children have larger ids.
        let mut below: Vec<u32> = listed.iter().map(|l| l.len() as u32).collect();
        for g in (1..n).rev() {
            let p = table.parent[g] as usize;
            below[p] += below[g];
        }
        let mut kids: Vec<(u32, u32)> = (1..n as u32)
            .filter(|&g| below[g as usize] > 0)
            .map(|g| (table.parent[g as usize], g))
            .collect();
        let kid_starts = counting_sort(&mut kids, n, |k| k.0);
        let name_of = |g: u32| {
            let p = table.paths[g as usize];
            &p[p.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1)..]
        };
        // Path order: a folder's name sorts as if followed by its '/'.
        let path_cmp = |a: &[u8], b: &[u8]| a.iter().chain(b"/").cmp(b.iter().chain(b"/"));
        for g in 0..n {
            kids[kid_starts[g]..kid_starts[g + 1]]
                .sort_unstable_by(|x, y| path_cmp(name_of(x.1), name_of(y.1)));
        }
        // How many of its folder's files sort before each subfolder.
        let files_before: Vec<u32> = (kids.par_iter())
            .map(|&(g, k)| {
                let l = &listed[g as usize];
                partition(l.len(), |i| file_first(l.name(i), name_of(k))) as u32
            })
            .collect();

        let files = below[0] as usize;
        // (global folder, first file, end) in FileId order.
        let mut runs: Vec<(u32, u32, u32)> = Vec::new();
        let mut folder_files = vec![FileId(0); files];
        // Global folder of each kept folder, in FolderId order, and where
        // each one's files start in `folder_files`.
        let mut kept: Vec<u32> = vec![0];
        let mut id_of = vec![u32::MAX; n];
        id_of[0] = 0;
        let mut files_at = vec![0u32; n];
        let (mut next_id, mut next_at) = (0u32, listed[0].len() as u32);
        // (global folder, next file, next subfolder)
        let mut stack: Vec<(u32, u32, usize)> = vec![(0, 0, kid_starts[0])];
        while let Some(&(g, fi, ki)) = stack.last() {
            let kid_end = kid_starts[g as usize + 1];
            let upto = if ki < kid_end {
                files_before[ki]
            } else {
                listed[g as usize].len() as u32
            };
            if fi < upto {
                runs.push((g, fi, upto));
                let at = files_at[g as usize] as usize;
                let slots = &mut folder_files[at + fi as usize..at + upto as usize];
                for (slot, id) in slots.iter_mut().zip(next_id..) {
                    *slot = FileId(id);
                }
                next_id += upto - fi;
            }
            let top = stack.last_mut().unwrap();
            top.1 = upto;
            if ki < kid_end {
                top.2 += 1;
                let sub = kids[ki].1;
                id_of[sub as usize] = kept.len() as u32;
                kept.push(sub);
                files_at[sub as usize] = next_at;
                next_at += listed[sub as usize].len() as u32;
                stack.push((sub, 0, kid_starts[sub as usize]));
            } else {
                stack.pop();
            }
        }

        // Runs in batches of about BATCH files, one task each, and what
        // each batch writes: files, name bytes, layers.
        const BATCH: usize = 1 << 13;
        let mut batches: Vec<(Range<usize>, [usize; 3])> = Vec::new();
        let mut first = 0;
        let mut size = [0usize; 3];
        for (r, &(g, a, b)) in runs.iter().enumerate() {
            let l = &listed[g as usize];
            let ((na, la), (nb, lb)) = (l.starts(a as usize), l.starts(b as usize));
            size = [
                size[0] + (b - a) as usize,
                size[1] + nb - na,
                size[2] + lb - la,
            ];
            if size[0] >= BATCH || r + 1 == runs.len() {
                batches.push((first..r + 1, size));
                (first, size) = (r + 1, [0; 3]);
            }
        }
        let total = |i: usize| batches.iter().map(|b| b.1[i]).sum::<usize>();
        let (name_bytes, layer_count) = (total(1), total(2));
        let mut name_starts = vec![0u32; files + 1];
        let mut layer_starts = vec![0u32; files + 1];
        let mut file_folder = vec![0u32; files];
        let mut names = vec![0u8; name_bytes];
        let mut layers: Vec<Layer> = Vec::with_capacity(layer_count);
        let pieces = |i: usize| batches.iter().map(move |b| b.1[i]);
        let jobs: Vec<_> = (batches.iter())
            .zip(split_lens(&mut name_starts[..files], pieces(0)))
            .zip(split_lens(&mut layer_starts[..files], pieces(0)))
            .zip(split_lens(&mut file_folder, pieces(0)))
            .zip(split_lens(&mut names, pieces(1)))
            .zip(split_lens(
                &mut layers.spare_capacity_mut()[..layer_count],
                pieces(2),
            ))
            .collect();
        let (mut name_base, mut layer_base) = (0, 0);
        let bases: Vec<(usize, usize)> = (batches.iter())
            .map(|b| {
                let base = (name_base, layer_base);
                (name_base, layer_base) = (name_base + b.1[1], layer_base + b.1[2]);
                base
            })
            .collect();
        jobs.into_par_iter()
            .zip(bases)
            .for_each(|(job, (name_base, layer_base))| {
                let (((((batch, name_starts), layer_starts), file_folder), names), layers) = job;
                let (mut f, mut nb, mut lb) = (0, 0, 0);
                for &(g, a, b) in &runs[batch.0.clone()] {
                    let l = &listed[g as usize];
                    let ((na, la), (ne, le)) = (l.starts(a as usize), l.starts(b as usize));
                    names[nb..nb + ne - na].copy_from_slice(&l.names[na..ne]);
                    layers[lb..lb + le - la].write_copy_of_slice(&l.layers[la..le]);
                    for k in a as usize..b as usize {
                        let (nk, lk) = l.starts(k);
                        name_starts[f] = (name_base + nb + nk - na) as u32;
                        layer_starts[f] = (layer_base + lb + lk - la) as u32;
                        file_folder[f] = id_of[g as usize];
                        f += 1;
                    }
                    (nb, lb) = (nb + ne - na, lb + le - la);
                }
            });
        // SAFETY: the batches' layer pieces cover all `layer_count` slots,
        // and each wrote its runs' layers, which are its piece's length.
        unsafe { layers.set_len(layer_count) };
        name_starts[files] = name_bytes as u32;
        layer_starts[files] = layer_count as u32;

        let mut folders = Vec::with_capacity(kept.len());
        let mut folder_paths = String::new();
        let mut children = Vec::with_capacity(kept.len().saturating_sub(1));
        for &g in &kept {
            let path = std::str::from_utf8(table.paths[g as usize]).expect("keys are UTF-8");
            let c0 = children.len() as u32;
            let mut subs: Vec<FolderId> = kids[kid_starts[g as usize]..kid_starts[g as usize + 1]]
                .iter()
                .map(|k| FolderId(id_of[k.1 as usize]))
                .collect();
            // The browser lists subfolders by name, not in path order.
            subs.sort_unstable_by(|a, b| {
                name_of(kept[a.0 as usize]).cmp(name_of(kept[b.0 as usize]))
            });
            children.extend_from_slice(&subs);
            let f0 = files_at[g as usize];
            folders.push(Folder {
                start: folder_paths.len() as u32,
                len: path.len() as u32,
                parent: if g == 0 {
                    u32::MAX
                } else {
                    id_of[table.parent[g as usize] as usize]
                },
                subfolders: (c0, children.len() as u32),
                files: (f0, f0 + listed[g as usize].len() as u32),
            });
            folder_paths.push_str(path);
        }

        self.names = String::from_utf8(names).expect("keys are UTF-8");
        self.name_starts = name_starts;
        self.file_folder = file_folder;
        self.layer_starts = layer_starts;
        self.layers = layers;
        self.folders = folders;
        self.folder_children = children;
        self.folder_files = folder_files;
        self.folder_paths = folder_paths;
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Changes when any archive's name, size or modification time does:
    /// what caches built from the install's files check, where a patch can
    /// keep the file count.
    pub fn stamp(&self) -> u64 {
        self.stamp
    }

    /// Mounted archive file names in mount order; index with [`ArchiveId`].
    pub fn archives(&self) -> &[String] {
        &self.archive_names
    }

    #[cfg(test)]
    fn archive(&self, id: ArchiveId) -> &Archive {
        &self.archives[id as usize]
    }

    /// Visible virtual paths.
    pub fn file_count(&self) -> usize {
        self.name_starts.len() - 1
    }

    /// Entries hidden by a later archive's copy of the same path.
    #[cfg(test)]
    fn shadowed_count(&self) -> usize {
        self.shadowed
    }

    /// Live entries whose path a later archive tombstoned.
    #[cfg(test)]
    fn deleted_count(&self) -> usize {
        self.deleted
    }

    /// Normalized path (lowercase, `/`), its folder's and its name joined:
    /// made each call. [`Install::file_name`] and [`Install::folder_path`]
    /// borrow.
    pub fn file_path(&self, id: FileId) -> String {
        let (dir, name) = (self.folder_path(self.file_folder(id)), self.file_name(id));
        if dir.is_empty() {
            name.to_owned()
        } else {
            format!("{dir}/{name}")
        }
    }

    pub fn file_name(&self, id: FileId) -> &str {
        let i = id.0 as usize;
        &self.names[self.name_starts[i] as usize..self.name_starts[i + 1] as usize]
    }

    /// How file `i`'s path sorts against `key`, without joining it.
    fn cmp_path(&self, i: usize, key: &str) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let id = FileId(i as u32);
        let (dir, name) = (
            self.folder_path(self.file_folder(id)).as_bytes(),
            self.file_name(id),
        );
        let mut key = key.as_bytes();
        if !dir.is_empty() {
            let n = dir.len().min(key.len());
            match dir[..n].cmp(&key[..n]) {
                Ordering::Equal if key.len() > dir.len() => {}
                Ordering::Equal => return Ordering::Greater,
                other => return other,
            }
            match b'/'.cmp(&key[dir.len()]) {
                Ordering::Equal => key = &key[dir.len() + 1..],
                other => return other,
            }
        }
        name.as_bytes().cmp(key)
    }

    pub fn file_folder(&self, id: FileId) -> FolderId {
        FolderId(self.file_folder[id.0 as usize])
    }

    /// Every archive that has a say about this path, in mount order. The last
    /// layer is the one the game reads.
    pub fn layers(&self, id: FileId) -> &[Layer] {
        let i = id.0 as usize;
        &self.layers[self.layer_starts[i] as usize..self.layer_starts[i + 1] as usize]
    }

    pub fn winner(&self, id: FileId) -> &Layer {
        self.layers(id).last().expect("indexed files have a layer")
    }

    /// Any spelling the engine would accept (case, `\`, `./`).
    pub fn file(&self, path: &str) -> Option<FileId> {
        use std::cmp::Ordering;
        let key = crate::rda::normalize_path(path);
        let n = self.file_count();
        let i = partition(n, |i| self.cmp_path(i, &key) == Ordering::Less);
        (i < n && self.cmp_path(i, &key) == Ordering::Equal).then_some(FileId(i as u32))
    }

    /// Serves the loose files of mod folders under `roots` (the game's
    /// project roots: `<mod>/data/...`) over the archives, replacing the
    /// previous ones, later folders winning: the project's dependencies
    /// first, the project last. Each file remembers which folder it came
    /// from ([`Install::mod_layer`]). XML the game ships is skipped,
    /// because the mod loader applies a mod's copy as ModOps patches, not as a
    /// replacement. XML the game doesn't have (a ported ground material, an
    /// island's own material set) is just a new file and is served. Only
    /// [`Install::read`], [`Install::exists`] and [`Install::list`] see these
    /// files; the browser tree and searches stay on the shipped index.
    pub fn mount_mods(&self, dirs: &[&Path], roots: &[&str]) -> Result<usize> {
        let mut files = BTreeMap::new();
        for (layer, dir) in dirs.iter().enumerate() {
            let layer = u16::try_from(layer).context("too many mod folders")?;
            // Only the roots are mounted; walking the rest would also walk a
            // mod's `.git` or nested mods.
            let mut stack: Vec<_> = roots.iter().map(|r| dir.join(r)).collect();
            while let Some(at) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&at) else {
                    continue;
                };
                for entry in entries {
                    let path = entry?.path();
                    if path.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    let rel = path.strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
                    if rel.ends_with(".xml") && self.file(&rel).is_some() {
                        continue;
                    }
                    files.insert(crate::rda::normalize_path(&rel).into_owned(), (layer, path));
                }
            }
        }
        let n = files.len();
        *self.overlay.write().expect("overlay lock") = files;
        Ok(n)
    }

    /// Which folder of the last [`Install::mount_mods`] serves `path`, as
    /// an index into the folders it was given.
    pub fn mod_layer(&self, path: &str) -> Option<usize> {
        let overlay = self.overlay.read().expect("overlay lock");
        overlay
            .get(crate::rda::normalize_path(path).as_ref())
            .map(|(layer, _)| usize::from(*layer))
    }

    /// Mounted paths that end in `suffix` and that folder `layer` serves.
    pub fn mod_files(&self, layer: usize, suffix: &str) -> Vec<String> {
        let overlay = self.overlay.read().expect("overlay lock");
        overlay
            .iter()
            .filter(|(p, (l, _))| usize::from(*l) == layer && p.ends_with(suffix))
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// The path is a shipped file or in the mounted mod.
    pub fn exists(&self, path: &str) -> bool {
        self.file(path).is_some() || self.overlay_file(path).is_some()
    }

    /// Paths of the files directly in folder `dir`, shipped and mounted,
    /// sorted. Read them with [`Install::read`].
    pub fn list(&self, dir: &str) -> Vec<String> {
        let key = crate::rda::normalize_path(dir);
        let prefix = format!("{}/", key.trim_end_matches('/'));
        let mut out: Vec<String> = self
            .folder(&key)
            .map(|f| {
                self.folder_files(f)
                    .iter()
                    .map(|&id| self.file_path(id))
                    .collect()
            })
            .unwrap_or_default();
        let overlay = self.overlay.read().expect("overlay lock");
        out.extend(
            overlay
                .range(prefix.clone()..)
                .take_while(|(p, _)| p.starts_with(&prefix))
                .filter(|(p, _)| !p[prefix.len()..].contains('/'))
                .map(|(p, _)| p.clone()),
        );
        out.sort();
        out.dedup();
        out
    }

    fn overlay_file(&self, path: &str) -> Option<PathBuf> {
        let overlay = self.overlay.read().expect("overlay lock");
        if overlay.is_empty() {
            return None;
        }
        overlay
            .get(crate::rda::normalize_path(path).as_ref())
            .map(|(_, file)| file.clone())
    }

    /// Winning bytes of `path`: the mounted mod's, else the archives'.
    /// Takes [`Install::file_path`]'s `String` as well as a `&str`.
    pub fn read(&self, path: impl AsRef<str>) -> Result<Vec<u8>> {
        let path = path.as_ref();
        if let Some(file) = self.overlay_file(path) {
            return std::fs::read(&file).with_context(|| format!("Reading {}", file.display()));
        }
        let Some(id) = self.file(path) else {
            bail!("{}: no file {path}", self.dir.display());
        };
        self.read_file(id)
    }

    /// Up to `len` bytes of `path` from `start`, without reading the rest
    /// where the archive stores the file as is: large textures need only
    /// their smaller mips.
    pub fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        if let Some(file) = self.overlay_file(path) {
            use std::io::{Read, Seek};
            let mut f = std::fs::File::open(&file)
                .with_context(|| format!("Reading {}", file.display()))?;
            f.seek(std::io::SeekFrom::Start(start))?;
            let mut out = Vec::new();
            f.take(len).read_to_end(&mut out)?;
            return Ok(out);
        }
        let Some(id) = self.file(path) else {
            bail!("{}: no file {path}", self.dir.display());
        };
        let layer = self.winner(id);
        let entry = self.entry(id, layer)?;
        self.archives[layer.archive as usize].read_range(&entry, start, len)
    }

    /// The archive entry of `id` in `layer`.
    fn entry(&self, id: FileId, layer: &Layer) -> Result<Entry> {
        ensure!(
            !layer.is_tombstone(),
            "{}: {} is a tombstone",
            self.archive_names[layer.archive as usize],
            self.file_path(id)
        );
        Ok(Entry {
            path: self.file_path(id).into(),
            offset: layer.offset,
            compressed_size: u64::from(layer.compressed_size),
            raw_size: u64::from(layer.raw_size),
            flags: layer.flags,
            memres: layer.memres,
        })
    }

    pub fn read_file(&self, id: FileId) -> Result<Vec<u8>> {
        self.read_layer(id, self.winner(id))
    }

    /// Bytes of an older layer, e.g. the shipped version under a patch.
    pub fn read_layer(&self, id: FileId, layer: &Layer) -> Result<Vec<u8>> {
        let entry = self.entry(id, layer)?;
        self.archives[layer.archive as usize].read(&entry)
    }

    pub fn root(&self) -> FolderId {
        FolderId(0)
    }

    /// Folder by path; `""` is the root.
    pub fn folder(&self, path: &str) -> Option<FolderId> {
        let key = crate::rda::normalize_path(path);
        if key.is_empty() {
            return Some(self.root());
        }
        // A folder's files are the contiguous run of paths prefixed "<key>/".
        let prefix = format!("{key}/");
        let n = self.file_count();
        let i = partition(n, |i| self.cmp_path(i, &prefix) == std::cmp::Ordering::Less);
        if i == n || !self.file_path(FileId(i as u32)).starts_with(&prefix) {
            return None;
        }
        let mut f = self.file_folder(FileId(i as u32));
        while self.folders[f.0 as usize].len as usize > key.len() {
            f = self.parent(f)?;
        }
        Some(f)
    }

    pub fn folder_path(&self, id: FolderId) -> &str {
        let f = &self.folders[id.0 as usize];
        &self.folder_paths[f.start as usize..][..f.len as usize]
    }

    pub fn folder_name(&self, id: FolderId) -> &str {
        let p = self.folder_path(id);
        &p[p.rfind('/').map_or(0, |i| i + 1)..]
    }

    pub fn parent(&self, id: FolderId) -> Option<FolderId> {
        let p = self.folders[id.0 as usize].parent;
        (p != u32::MAX).then_some(FolderId(p))
    }

    /// Direct subfolders, sorted by name.
    pub fn subfolders(&self, id: FolderId) -> &[FolderId] {
        let (a, b) = self.folders[id.0 as usize].subfolders;
        &self.folder_children[a as usize..b as usize]
    }

    /// Direct files, sorted by name.
    pub fn folder_files(&self, id: FolderId) -> &[FileId] {
        let (a, b) = self.folders[id.0 as usize].files;
        &self.folder_files[a as usize..b as usize]
    }

    /// Browser rows: subfolders first, then files, each sorted by name.
    pub fn children(&self, id: FolderId) -> impl Iterator<Item = Node> + '_ {
        let folders = self.subfolders(id).iter().map(|&f| Node::Folder(f));
        let files = self.folder_files(id).iter().map(|&f| Node::File(f));
        folders.chain(files)
    }

    /// Case-insensitive substring match over full paths, in path order, at
    /// most `limit` hits.
    pub fn search(&self, query: &str, limit: usize) -> Vec<FileId> {
        let query = query.replace('\\', "/").to_ascii_lowercase();
        let mut hits = Vec::new();
        if query.is_empty() || query.contains('\0') || limit == 0 {
            return hits;
        }
        // Within a name: one pass over the names, a hit kept when it ends
        // inside the name it starts in. Within a folder's path and the '/'
        // after it: every file of the folder. Across that '/' into a name:
        // a suffix of the folder's path and '/' is a prefix of the query.
        let n = self.file_count();
        let mut hit = vec![0u64; n.div_ceil(64)];
        for (at, _) in self.names.match_indices(query.as_str()) {
            let i = partition(n, |i| self.name_starts[i + 1] as usize <= at);
            if at + query.len() <= self.name_starts[i + 1] as usize {
                hit[i / 64] |= 1 << (i % 64);
            }
        }
        let mut dir = String::new();
        for f in 1..self.folders.len() as u32 {
            dir.clear();
            dir.push_str(self.folder_path(FolderId(f)));
            dir.push('/');
            let files = self.folder_files(FolderId(f));
            if dir.contains(query.as_str()) {
                for &id in files {
                    hit[id.0 as usize / 64] |= 1 << (id.0 % 64);
                }
                continue;
            }
            for k in (1..query.len()).filter(|&k| query.is_char_boundary(k)) {
                if !dir.ends_with(&query[..k]) {
                    continue;
                }
                for &id in files {
                    if self.file_name(id).starts_with(&query[k..]) {
                        hit[id.0 as usize / 64] |= 1 << (id.0 % 64);
                    }
                }
            }
        }
        for (w, &bits) in hit.iter().enumerate() {
            let mut bits = bits;
            while bits != 0 && hits.len() < limit {
                hits.push(FileId((w * 64 + bits.trailing_zeros() as usize) as u32));
                bits &= bits - 1;
            }
        }
        hits
    }
}

// Header records parsed flat: keys split at their last '/' into a folder,
// interned, and a name.
#[derive(Default)]
struct Records {
    // Folder f's key ends at folder_ends[f], back to back.
    folder_keys: Vec<u8>,
    folder_ends: Vec<u32>,
    folder_ids: HashMap<Box<[u8]>, u32, FixedState>,
    names: Vec<u8>,
    entries: Vec<Parsed>,
    // Of each entry's whole key.
    hashes: Vec<u64>,
}

#[derive(Clone, Copy)]
struct Parsed {
    layer: Layer,
    // The name runs from the previous entry's end to this one.
    name_end: u32,
    folder: u32,
}

impl Records {
    // Adds the records of one header table.
    fn add(&mut self, archive: ArchiveId, t: &Table) -> Result<()> {
        self.entries.reserve(t.len());
        self.hashes.reserve(t.len());
        let hasher = FixedState::default();
        let mut key = Vec::with_capacity(512);
        let mut last = self.entries.last().map_or(u32::MAX, |e| e.folder);
        for i in 0..t.len() {
            let r = t.record(i)?;
            key.clear();
            let cut = crate::rda::push_key(r.path, &mut key);
            let (folder, name) = match cut {
                Some(c) => (&key[..c], &key[c + 1..]),
                None => (&key[..0], &key[..]),
            };
            // Blocks are sorted, so runs of entries share their folder.
            if last == u32::MAX || self.folder(last as usize) != folder {
                last = self.intern(folder);
            }
            let size = |v: u64| u32::try_from(v).ok();
            let (Some(compressed_size), Some(raw_size)) =
                (size(r.compressed_size), size(r.raw_size))
            else {
                bail!(
                    "{}: files of 4 GB and over are not supported",
                    String::from_utf8_lossy(&key)
                );
            };
            self.hashes.push(hasher.hash_one(&key[..]));
            self.names.extend_from_slice(name);
            self.entries.push(Parsed {
                layer: Layer {
                    archive,
                    flags: t.flags,
                    offset: r.offset,
                    compressed_size,
                    raw_size,
                    memres: t.memres,
                },
                name_end: self.names.len() as u32,
                folder: last,
            });
        }
        ensure!(
            self.names.len() < u32::MAX as usize,
            "name arena over 4 GiB"
        );
        Ok(())
    }

    fn intern(&mut self, folder: &[u8]) -> u32 {
        if let Some(&f) = self.folder_ids.get(folder) {
            return f;
        }
        self.folder_keys.extend_from_slice(folder);
        self.folder_ends.push(self.folder_keys.len() as u32);
        let f = self.folder_ends.len() as u32 - 1;
        self.folder_ids.insert(folder.into(), f);
        f
    }

    // A bitset of the entries no later entry repeats the key of: the
    // engine's map keeps one per key. No shipped archive repeats one, but
    // a mod or patch may.
    fn last_of_each_key(&self) -> Vec<u64> {
        let n = self.entries.len();
        let mut kept = vec![0u64; n.div_ceil(64)];
        // Open addressing over entry indices, + 1 so 0 is empty.
        let cap = (2 * n).next_power_of_two().max(16);
        let mut slots = vec![0u32; cap];
        let hashes = &self.hashes;
        for i in 0..n {
            let mut at = hashes[i] as usize & (cap - 1);
            loop {
                let slot = slots[at];
                if slot == 0 {
                    slots[at] = i as u32 + 1;
                    kept[i / 64] |= 1 << (i % 64);
                    break;
                }
                let j = slot as usize - 1;
                if hashes[j] == hashes[i]
                    && self.entries[j].folder == self.entries[i].folder
                    && self.name(j) == self.name(i)
                {
                    slots[at] = i as u32 + 1;
                    kept[j / 64] &= !(1 << (j % 64));
                    kept[i / 64] |= 1 << (i % 64);
                    break;
                }
                at = (at + 1) & (cap - 1);
            }
        }
        kept
    }

    fn folder_count(&self) -> usize {
        self.folder_ends.len()
    }

    fn folder(&self, f: usize) -> &[u8] {
        let from = if f == 0 {
            0
        } else {
            self.folder_ends[f - 1] as usize
        };
        &self.folder_keys[from..self.folder_ends[f] as usize]
    }

    fn name(&self, i: usize) -> &[u8] {
        let from = if i == 0 {
            0
        } else {
            self.entries[i - 1].name_end as usize
        };
        &self.names[from..self.entries[i].name_end as usize]
    }
}

// One archive's records by folder, only each key's last in the chain kept.
struct Scanned {
    archive: Archive,
    records: Records,
    // Kept entries by folder, each folder's sorted by name.
    by_folder: Vec<u32>,
    folder_starts: Vec<usize>,
}

impl Scanned {
    // One task per archive, walking its chain and parsing as it goes.
    // Cutting the big archives (1800's have 84k records) into chunks on
    // all cores measured slower: the walk's per-block tables and the
    // chunks' copies cost more fresh memory than the balance won.
    fn parse(archive: ArchiveId, path: &Path) -> Result<Scanned> {
        let mapped = Mapped::open(path)?;
        let mut records = Records::default();
        let sections = mapped.walk(|t| records.add(archive, &t))?;
        let archive = mapped.into_archive(sections);
        let kept = records.last_of_each_key();
        records.hashes = Vec::new();
        records.folder_ids = HashMap::default();
        let mut by_folder: Vec<u32> = (0..records.entries.len() as u32)
            .filter(|&i| kept[i as usize / 64] & (1 << (i % 64)) != 0)
            .collect();
        let folder_starts = counting_sort(&mut by_folder, records.folder_count(), |&i| {
            records.entries[i as usize].folder
        });
        for f in 0..records.folder_count() {
            let run = &mut by_folder[folder_starts[f]..folder_starts[f + 1]];
            let name = |&i: &u32| records.name(i as usize);
            if !run.is_sorted_by(|x, y| name(x) <= name(y)) {
                run.sort_unstable_by(|x, y| name(x).cmp(name(y)));
            }
        }
        Ok(Scanned {
            archive,
            records,
            by_folder,
            folder_starts,
        })
    }

    fn folder_count(&self) -> usize {
        self.records.folder_count()
    }

    fn folder(&self, f: usize) -> &[u8] {
        self.records.folder(f)
    }

    fn in_folder(&self, f: usize) -> &[u32] {
        &self.by_folder[self.folder_starts[f]..self.folder_starts[f + 1]]
    }

    fn name(&self, i: usize) -> &[u8] {
        self.records.name(i)
    }

    fn layer(&self, i: usize) -> &Layer {
        &self.records.entries[i].layer
    }
}

// Every archive's folders, interned. Paths borrow the archives' keys.
#[derive(Default)]
struct FolderTable<'a> {
    ids: HashMap<&'a [u8], u32, FixedState>,
    paths: Vec<&'a [u8]>,
    parent: Vec<u32>,
}

impl<'a> FolderTable<'a> {
    // Interns the parent first, so parents get the smaller ids.
    fn intern(&mut self, path: &'a [u8]) -> u32 {
        if let Some(&id) = self.ids.get(path) {
            return id;
        }
        let parent = if path.is_empty() {
            u32::MAX
        } else {
            let cut = path.iter().rposition(|&b| b == b'/').unwrap_or(0);
            self.intern(&path[..cut])
        };
        let id = self.paths.len() as u32;
        self.ids.insert(path, id);
        self.paths.push(path);
        self.parent.push(parent);
        id
    }
}

// One folder's visible files across all archives, by name, and their layers.
#[derive(Default)]
struct Listed {
    // File i's name ends at name_ends[i], back to back.
    names: Vec<u8>,
    name_ends: Vec<u32>,
    // File i's layers end at layer_ends[i], back to back.
    layer_ends: Vec<u32>,
    layers: Vec<Layer>,
    shadowed: usize,
    deleted: usize,
}

impl Listed {
    // `sources` are the folder's (folder, archive, local folder) in mount
    // order. A big folder (1800's shader cache holds half its entries) is
    // cut by name and the pieces merged on all cores; a name's entries
    // always land in one piece.
    fn layer<'a>(scanned: &'a [Scanned], sources: &[(u32, ArchiveId, u32)]) -> Listed {
        use rayon::prelude::*;
        let runs: Vec<(&'a Scanned, &'a [u32])> = (sources.iter())
            .map(|&(_, a, l)| {
                (
                    &scanned[a as usize],
                    scanned[a as usize].in_folder(l as usize),
                )
            })
            .collect();
        let total: usize = runs.iter().map(|r| r.1.len()).sum();
        // About 8,192 entries a piece, at most 256 pieces.
        const PIECE: usize = 1 << 13;
        const MAX_PIECES: usize = 256;
        let pieces = (total / PIECE).min(MAX_PIECES);
        if pieces < 2 {
            return Listed::merge(&runs);
        }
        // Cuts at quantiles of a sample of every run: runs can cover
        // different name ranges (1800's base shaders and their patches').
        let mut sample: Vec<&[u8]> = Vec::with_capacity(pieces * runs.len());
        for &(sc, run) in &runs {
            let step = (run.len() / pieces).max(1);
            sample.extend(run.iter().step_by(step).map(|&e| sc.name(e as usize)));
        }
        sample.sort_unstable();
        let cuts: Vec<&[u8]> = (1..pieces)
            .map(|p| sample[p * sample.len() / pieces])
            .collect();
        let bounds: Vec<Vec<usize>> = (runs.iter())
            .map(|&(sc, run)| {
                let mut b = vec![0];
                b.extend(
                    cuts.iter()
                        .map(|&c| run.partition_point(|&e| sc.name(e as usize) < c)),
                );
                b.push(run.len());
                b
            })
            .collect();
        let parts: Vec<Listed> = (0..pieces)
            .into_par_iter()
            .map(|p| {
                let sub: Vec<(&'a Scanned, &'a [u32])> = (runs.iter().zip(&bounds))
                    .map(|(&(sc, run), b)| (sc, &run[b[p]..b[p + 1]]))
                    .collect();
                Listed::merge(&sub)
            })
            .collect();
        let mut out = Listed::default();
        for part in parts {
            let (names, base) = (out.names.len() as u32, out.layers.len() as u32);
            out.names.extend_from_slice(&part.names);
            out.name_ends
                .extend(part.name_ends.iter().map(|e| e + names));
            out.layer_ends
                .extend(part.layer_ends.iter().map(|e| e + base));
            out.layers.extend_from_slice(&part.layers);
            out.shadowed += part.shadowed;
            out.deleted += part.deleted;
        }
        out
    }

    // Each archive's run is sorted by name and has a name once, so a
    // stable sort by name merges the runs and keeps each name's entries in
    // mount order: its layers.
    fn merge<'a>(runs: &[(&'a Scanned, &'a [u32])]) -> Listed {
        let mut refs: Vec<(&'a [u8], &'a Layer)> =
            Vec::with_capacity(runs.iter().map(|r| r.1.len()).sum());
        for &(sc, run) in runs {
            refs.extend(
                run.iter()
                    .map(|&e| (sc.name(e as usize), sc.layer(e as usize))),
            );
        }
        if runs.len() > 1 {
            refs.sort_by(|x, y| x.0.cmp(y.0));
        }
        let mut out = Listed::default();
        for group in refs.chunk_by(|x, y| x.0 == y.0) {
            let (name, last) = *group.last().unwrap();
            if last.is_tombstone() {
                out.deleted += group.iter().filter(|g| !g.1.is_tombstone()).count();
                continue;
            }
            out.shadowed += group.len() - 1;
            out.layers.extend(group.iter().map(|g| *g.1));
            out.names.extend_from_slice(name);
            out.name_ends.push(out.names.len() as u32);
            out.layer_ends.push(out.layers.len() as u32);
        }
        out
    }

    fn len(&self) -> usize {
        self.name_ends.len()
    }

    // Where file i's name and layers start.
    fn starts(&self, i: usize) -> (usize, usize) {
        match i {
            0 => (0, 0),
            _ => (
                self.name_ends[i - 1] as usize,
                self.layer_ends[i - 1] as usize,
            ),
        }
    }

    fn name(&self, i: usize) -> &[u8] {
        &self.names[self.starts(i).0..self.name_ends[i] as usize]
    }
}

// A file named `name` sorts before the files of subfolder `sub`.
fn file_first(name: &[u8], sub: &[u8]) -> bool {
    name.iter().lt(sub.iter().chain(b"/"))
}

// `out` cut into consecutive pieces of the given lengths.
fn split_lens<T>(mut out: &mut [T], lens: impl Iterator<Item = usize>) -> Vec<&mut [T]> {
    let mut pieces = Vec::new();
    for len in lens {
        let (piece, rest) = std::mem::take(&mut out).split_at_mut(len);
        pieces.push(piece);
        out = rest;
    }
    pieces
}

// Stable counting sort of `items` into `buckets` by `key`; returns where
// each bucket starts, and the end.
fn counting_sort<T: Copy>(
    items: &mut Vec<T>,
    buckets: usize,
    key: impl Fn(&T) -> u32,
) -> Vec<usize> {
    let mut starts = vec![0usize; buckets + 1];
    for t in items.iter() {
        starts[key(t) as usize + 1] += 1;
    }
    for b in 0..buckets {
        starts[b + 1] += starts[b];
    }
    let mut at = starts.clone();
    let mut out = Vec::with_capacity(items.len());
    let spare = out.spare_capacity_mut();
    for &t in items.iter() {
        let b = key(&t) as usize;
        spare[at[b]].write(t);
        at[b] += 1;
    }
    // SAFETY: every slot was written: the buckets' counts sum to the length.
    unsafe { out.set_len(items.len()) };
    *items = out;
    starts
}

// Wall time of each step of one mount, for `index_build_phases`.
struct Phases {
    at: Instant,
    steps: Vec<(&'static str, Duration)>,
}

impl Default for Phases {
    fn default() -> Phases {
        Phases {
            at: Instant::now(),
            steps: Vec::new(),
        }
    }
}

impl Phases {
    fn mark(&mut self, step: &'static str) {
        let now = Instant::now();
        self.steps.push((step, now - self.at));
        self.at = now;
    }
}

// The install folder, then each archive's name, size and modification
// time, hashed with FNV-1a: the same in every run, and over the same bytes
// the stored index was keyed by, so stamps kept on disk (the UI's file
// index) still match.
fn stamp(dir: &Path, files: &[PathBuf]) -> Result<u64> {
    let mut key = Vec::new();
    let text = |key: &mut Vec<u8>, s: &str| {
        key.extend_from_slice(&(s.len() as u64).to_le_bytes());
        key.extend_from_slice(s.as_bytes());
    };
    text(&mut key, &dir.to_string_lossy());
    key.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for path in files {
        let meta = std::fs::metadata(path).with_context(|| path.display().to_string())?;
        let modified = (meta.modified()?)
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        text(
            &mut key,
            &path.file_name().unwrap_or_default().to_string_lossy(),
        );
        key.extend_from_slice(&meta.len().to_le_bytes());
        key.extend_from_slice(&(modified.as_nanos() as u64).to_le_bytes());
    }
    Ok((key.iter()).fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    }))
}

// First index in 0..n where `before` is false; `before` must be monotone.
fn partition(n: usize, before: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if before(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// For tests: the game folder environment variable `var` names, or `None`
/// so the test skips. With `ANNO_REQUIRE_INSTALLS=1` a missing one fails
/// the test instead: a run meant to cover the installs can't pass by
/// skipping them.
pub fn install_dir(var: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(dir) => Some(dir.into()),
        None if std::env::var_os("ANNO_REQUIRE_INSTALLS").is_some_and(|v| v == "1") => {
            panic!("{var} is not set, and ANNO_REQUIRE_INSTALLS=1 asks for every install")
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rda::{PackOptions, Packer};
    use std::time::Instant;

    fn pack(dir: &Path, name: &str, files: &[(&str, &[u8])], tombs: &[&str]) {
        let mut p = Packer::new(PackOptions::default());
        for (path, data) in files {
            p.add(path, data).unwrap();
        }
        for t in tombs {
            p.tombstone(t).unwrap();
        }
        std::fs::write(dir.join("maindata").join(name), p.encode().unwrap()).unwrap();
    }

    // 2070's names: capitalised data archives, languages, patches, addons,
    // and a text checksums.rda; each family mounts in the order asked,
    // numbers within it counted, not compared as text.
    #[test]
    fn archives_mount_by_family_then_number() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir(d.join("maindata")).unwrap();
        for name in [
            "Data0.rda",
            "Data10.rda",
            "Data2.rda",
            "eng0.rda",
            "patch1.rda",
            "patch9.rda",
            "addon0.rda",
            "exedata.rda",
        ] {
            pack(d, name, &[("data/who.txt", name.as_bytes())], &[]);
        }
        std::fs::write(d.join("maindata/checksums.rda"), b"47159fea maindata/x").unwrap();
        let families = ["data", "eng", "patch", "addon", "exedata"];
        let install = Install::open(d, &families).unwrap();
        assert_eq!(
            install.archive_names,
            [
                "Data0.rda",
                "Data2.rda",
                "Data10.rda",
                "eng0.rda",
                "patch1.rda",
                "patch9.rda",
                "addon0.rda",
                "exedata.rda",
            ]
        );
        assert_eq!(install.read("data/who.txt").unwrap(), b"exedata.rda");
        // One family first, the rest by name.
        let data_first = Install::open(d, &["data"]).unwrap();
        assert_eq!(data_first.archive_names[2], "Data10.rda");
        assert_eq!(data_first.archive_names[3], "addon0.rda");
    }

    fn synthetic() -> (tempfile::TempDir, Install) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("maindata")).unwrap();
        let d = dir.path();
        pack(
            d,
            "config.rda",
            &[
                ("data/config/a.xml", b"base a"),
                ("data/config/b.xml", b"base b"),
                ("data/b-c/x.txt", b"x"),
                ("data/b/y.txt", b"y"),
                ("readme.txt", b"root file"),
            ],
            &[],
        );
        pack(
            d,
            "zz_patchfiles_150.rda",
            &[("Data/Config/A.xml", b"patch a")],
            &["data/config/b.xml"],
        );
        // `_` sorts after digits: this mod mounts after _150.
        pack(
            d,
            "zz_patchfiles_999_mod.rda",
            &[("data/config/a.xml", b"mod a")],
            &[],
        );
        // Digits before `_`: this one mounts before every zz_patchfiles_*.
        pack(
            d,
            "zz00_early.rda",
            &[("data/config/a.xml", b"early a")],
            &[],
        );
        std::fs::write(d.join("maindata/file_h.db"), b"not an archive").unwrap();
        let install = Install::open(d, Install::DATA_FIRST).unwrap();
        (dir, install)
    }

    #[test]
    fn a_patched_archive_changes_the_stamp() {
        let (game, fresh) = synthetic();
        let again = Install::open(game.path(), Install::DATA_FIRST).unwrap();
        assert_eq!(again.stamp(), fresh.stamp(), "the same every open");
        pack(
            game.path(),
            "zz00_early.rda",
            &[("data/config/new.xml", b"new")],
            &[],
        );
        let patched = Install::open(game.path(), Install::DATA_FIRST).unwrap();
        assert_eq!(patched.read("data/config/new.xml").unwrap(), b"new");
        assert_ne!(patched.stamp(), fresh.stamp());
        // A patch that keeps the file count still changes the stamp, which
        // caches built from the install check.
        pack(
            game.path(),
            "zz00_early.rda",
            &[("data/config/new.xml", b"newer")],
            &[],
        );
        let repatched = Install::open(game.path(), Install::DATA_FIRST).unwrap();
        assert_eq!(repatched.file_count(), patched.file_count());
        assert_ne!(repatched.stamp(), patched.stamp());
    }

    #[test]
    fn later_mod_folders_win_and_each_file_knows_its_folder() {
        let (_game, install) = synthetic();
        let mods = tempfile::tempdir().unwrap();
        let write = |m: &str, rel: &str, bytes: &[u8]| {
            let path = mods.path().join(m).join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        };
        write("dep", "data/graphics/tree.cfg", b"dep tree");
        write("dep", "data/graphics/rock.cfg", b"dep rock");
        write("project", "data/graphics/rock.cfg", b"project rock");
        // Outside `data/`: never mounted (a nested mod, or a `.git`).
        write("project", "nested/data/graphics/x.cfg", b"nested");
        // A patch of shipped XML stays the game's; the mod's own XML serves.
        write("project", "data/config/a.xml", b"<ModOps/>");
        write("project", "data/graphics/sets/own.xml", b"own set");
        let (dep, project) = (mods.path().join("dep"), mods.path().join("project"));
        // Paths, not files: both folders have rock.cfg.
        assert_eq!(install.mount_mods(&[&dep, &project], &["data"]).unwrap(), 3);
        assert_eq!(
            install.read("data/graphics/sets/own.xml").unwrap(),
            b"own set"
        );
        assert_eq!(
            install.read("data/graphics/rock.cfg").unwrap(),
            b"project rock"
        );
        assert_eq!(install.read("data/graphics/tree.cfg").unwrap(), b"dep tree");
        assert_eq!(install.mod_layer("Data/Graphics/Tree.cfg"), Some(0));
        assert_eq!(install.mod_layer("data/graphics/rock.cfg"), Some(1));
        assert_eq!(install.mod_layer("data/config/a.xml"), None, "shipped");
        assert_eq!(install.mod_files(0, ".cfg"), ["data/graphics/tree.cfg"]);
        assert!(!install.exists("nested/data/graphics/x.cfg"));
        // Mounting again replaces the set.
        install.mount_mods(&[project.as_path()], &["data"]).unwrap();
        assert!(!install.exists("data/graphics/tree.cfg"));
    }

    #[test]
    fn ranges_read_the_winning_bytes() {
        let (_dir, i) = synthetic();
        // The mod's "mod a" wins; ranges past the end stop there.
        assert_eq!(i.read_range("data/config/a.xml", 2, 3).unwrap(), b"d a");
        assert_eq!(i.read_range("data/config/a.xml", 4, 100).unwrap(), b"a");
        assert_eq!(
            i.read_range("readme.txt", 0, u64::MAX).unwrap(),
            b"root file"
        );
        assert!(
            i.read_range("data/config/b.xml", 0, 1).is_err(),
            "tombstone"
        );
    }

    #[test]
    fn mount_order_last_wins_and_tombstones_hide() {
        let (_dir, i) = synthetic();
        assert_eq!(
            i.archives(),
            [
                "config.rda",
                "zz00_early.rda",
                "zz_patchfiles_150.rda",
                "zz_patchfiles_999_mod.rda"
            ]
        );
        assert_eq!(i.read("DATA\\config\\a.xml").unwrap(), b"mod a");
        let a = i.file("data/config/a.xml").unwrap();
        let history: Vec<_> = i
            .layers(a)
            .iter()
            .map(|l| i.archives()[l.archive as usize].as_str())
            .collect();
        assert_eq!(
            history,
            [
                "config.rda",
                "zz00_early.rda",
                "zz_patchfiles_150.rda",
                "zz_patchfiles_999_mod.rda"
            ]
        );
        assert_eq!(i.read_layer(a, &i.layers(a)[0]).unwrap(), b"base a");
        assert!(i.file("data/config/b.xml").is_none());
        assert_eq!(i.deleted_count(), 1);
        assert_eq!(i.shadowed_count(), 3);
        assert_eq!(i.file_count(), 4);
    }

    #[test]
    fn folder_tree_and_search() {
        let (_dir, i) = synthetic();
        let root = i.root();
        let rows: Vec<String> = i
            .children(root)
            .map(|n| match n {
                Node::Folder(f) => format!("{}/", i.folder_name(f)),
                Node::File(f) => i.file_name(f).to_string(),
            })
            .collect();
        assert_eq!(rows, ["data/", "readme.txt"]);
        let data = i.folder("data").unwrap();
        let subs: Vec<_> = i
            .subfolders(data)
            .iter()
            .map(|&f| i.folder_name(f))
            .collect();
        assert_eq!(subs, ["b", "b-c", "config"]);
        let config = i.folder("Data/Config/").unwrap();
        assert_eq!(i.folder_path(config), "data/config");
        assert_eq!(i.parent(config), Some(data));
        assert_eq!(i.folder_files(config).len(), 1);
        assert!(i.folder("data/conf").is_none());
        assert!(i.folder("data/config/a.xml").is_none());
        assert_eq!(i.folder(""), Some(root));

        let hits: Vec<_> = i
            .search(".TXT", 10)
            .into_iter()
            .map(|f| i.file_path(f))
            .collect();
        assert_eq!(hits, ["data/b-c/x.txt", "data/b/y.txt", "readme.txt"]);
        assert_eq!(i.search("txt", 2).len(), 2);
        assert!(i.search("nothing", 5).is_empty());
        // No match across the \0 between two paths.
        assert!(i.search("txtdata", 5).is_empty());
    }

    #[test]
    fn numbered_1800_archives_override_in_numeric_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("maindata")).unwrap();
        pack(
            dir.path(),
            "data2.rda",
            &[("data/shared.txt", b"two"), ("data/deleted.txt", b"old")],
            &[],
        );
        pack(
            dir.path(),
            "data9.rda",
            &[("data/shared.txt", b"nine")],
            &[],
        );
        pack(
            dir.path(),
            "data10.rda",
            &[("data/shared.txt", b"ten")],
            &["data/deleted.txt"],
        );
        let install = Install::open(dir.path(), Install::DATA_FIRST).unwrap();
        assert_eq!(install.archives(), ["data2.rda", "data9.rda", "data10.rda"]);
        assert_eq!(install.read("data/shared.txt").unwrap(), b"ten");
        assert!(install.file("data/deleted.txt").is_none());
        let id = install.file("data/shared.txt").unwrap();
        assert_eq!(
            install.read_layer(id, &install.layers(id)[0]).unwrap(),
            b"two"
        );
    }

    #[test]
    fn installed_1800_browses_and_reads_real_assets() {
        let Some(dir) = crate::vfs::install_dir("ANNO1800_DIR") else {
            return;
        };
        let install = Install::open(&dir, Install::DATA_FIRST).unwrap();
        assert!(install.file_count() > 100_000);
        let asset = install
            .file("data/config/export/main/asset/assets.xml")
            .unwrap();
        assert!(install.winner(asset).raw_size > 1_000_000);
        // Independent check against this build's file.db ArchiveFiles table.
        let data9 = install
            .archives()
            .iter()
            .position(|a| a == "data9.rda")
            .unwrap();
        let data10 = install
            .archives()
            .iter()
            .position(|a| a == "data10.rda")
            .unwrap();
        assert!(data9 < data10);
        let texture = install.search(".dds", 1)[0];
        assert!(install.read_file(texture).unwrap().starts_with(b"DDS "));
        let ui = install
            .search("/ui/studio/generated/", 100)
            .into_iter()
            .find(|&id| !install.file_name(id).contains('.'))
            .unwrap();
        assert!(install.read_file(ui).unwrap().starts_with(b"PHX"));
        let island = install
            .search(".a7m", 100)
            .into_iter()
            .find(|&id| install.file_path(id).ends_with(".a7m"))
            .unwrap();
        let archive = Archive::parse(install.read_file(island).unwrap(), "1800 island").unwrap();
        assert!(!archive.read_path("gamedata.data").unwrap().is_empty());
        assert!(!archive.read_path("rd3d.data").unwrap().is_empty());
    }

    // Needs the game: every file and folder of an install is found by its
    // own path, and search hits what a scan over the joined paths does.
    #[test]
    fn real_install_finds_what_it_lists() {
        let Some(dir) = crate::vfs::install_dir("ANNO117_DIR") else {
            return;
        };
        let i = Install::open(dir, Install::DATA_FIRST).unwrap();
        let paths: Vec<String> = (0..i.file_count() as u32)
            .map(|f| i.file_path(FileId(f)))
            .collect();
        assert!(paths.windows(2).all(|w| w[0] < w[1]), "paths sort");
        for (f, path) in paths.iter().enumerate() {
            assert_eq!(i.file(path), Some(FileId(f as u32)), "{path}");
            assert_eq!(
                i.file_name(FileId(f as u32)),
                path.rsplit('/').next().unwrap()
            );
        }
        assert_eq!(i.file("data/nothing/here.xml"), None);
        for f in 1..i.folders.len() as u32 {
            assert_eq!(i.folder(i.folder_path(FolderId(f))), Some(FolderId(f)));
        }
        for query in [
            "roman_island",
            "/props/",
            "s/gr",
            ".a7m",
            "diffuse.dds",
            "zz",
        ] {
            let scan: Vec<FileId> = (paths.iter().enumerate())
                .filter(|(_, p)| p.contains(query))
                .map(|(f, _)| FileId(f as u32))
                .take(5000)
                .collect();
            assert_eq!(i.search(query, 5000), scan, "{query}");
        }
    }

    // Needs the games: each install lists, file for file and layer for
    // layer, what its archives' own entry lists give layered the plain way
    // (every key of every archive sorted, the last say winning), as the
    // index was built before it was parsed into flat arrays.
    #[test]
    fn real_installs_index_what_their_archives_list() {
        for var in ["ANNO117_DIR", "ANNO1800_DIR", "ANNO2070_DIR"] {
            let Some(dir) = crate::vfs::install_dir(var) else {
                continue;
            };
            let install = Install::open(&dir, Install::DATA_FIRST).unwrap();
            let files = Install::archive_files(&dir, Install::DATA_FIRST).unwrap();
            // (key, archive, offset, compressed size, raw size, flags)
            let mut all: Vec<(String, u16, u64, u64, u64, u32)> = Vec::new();
            for (a, path) in files.iter().enumerate() {
                let archive = Archive::open(path).unwrap();
                for e in archive.entries().iter().chain(archive.tombstones()) {
                    let fields = (e.offset, e.compressed_size, e.raw_size, e.flags.0);
                    all.push((
                        e.key().into_owned(),
                        a as u16,
                        fields.0,
                        fields.1,
                        fields.2,
                        fields.3,
                    ));
                }
            }
            all.sort_unstable();
            let mut want = all
                .chunk_by(|x, y| x.0 == y.0)
                .filter(|g| g.last().unwrap().5 & BlockFlags::DELETED == 0);
            for f in 0..install.file_count() as u32 {
                let id = FileId(f);
                let group = want
                    .next()
                    .unwrap_or_else(|| panic!("{var}: extra {}", install.file_path(id)));
                assert_eq!(install.file_path(id), group[0].0, "{var}");
                let layers: Vec<_> = (install.layers(id).iter())
                    .map(|l| {
                        (
                            l.archive,
                            l.offset,
                            l.compressed_size.into(),
                            l.raw_size.into(),
                            l.flags.0,
                        )
                    })
                    .collect();
                let expected: Vec<_> = group.iter().map(|g| (g.1, g.2, g.3, g.4, g.5)).collect();
                assert_eq!(layers, expected, "{var}: {}", group[0].0);
            }
            assert!(want.next().is_none(), "{var}: files missing");
        }
    }

    // Best of `INDEX_RUNS` (3) mounts of each installed game, step by step,
    // warm cache: `cargo test --release -p anno-formats index_build_phases
    // -- --ignored --nocapture`. Major faults mean the archives' header
    // tables were not all in the page cache. Linux only: getrusage and
    // /proc/loadavg.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn index_build_phases() {
        // (user, system CPU, major faults, minor faults) so far.
        fn usage() -> (Duration, Duration, i64, i64) {
            let mut u: libc::rusage = unsafe { std::mem::zeroed() };
            // SAFETY: `u` is a valid out pointer.
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
            let t = |v: libc::timeval| {
                Duration::from_micros(v.tv_sec as u64 * 1_000_000 + v.tv_usec as u64)
            };
            (t(u.ru_utime), t(u.ru_stime), u.ru_majflt, u.ru_minflt)
        }
        let runs = std::env::var("INDEX_RUNS").map_or(3, |r| r.parse().unwrap());
        let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        eprintln!(
            "load average {}",
            load.split(' ').take(3).collect::<Vec<_>>().join(" ")
        );
        for var in ["ANNO117_DIR", "ANNO1800_DIR"] {
            let Some(dir) = crate::vfs::install_dir(var) else {
                continue;
            };
            let mut best: Option<(Duration, String)> = None;
            for _ in 0..runs {
                let mut phases = Phases::default();
                let (user, sys, major, minor) = usage();
                let t = Instant::now();
                let files = Install::archive_files(&dir, Install::DATA_FIRST).unwrap();
                phases.mark("list maindata");
                let i = Install::mount_timed(dir.clone(), files, &mut phases).unwrap();
                drop(i);
                phases.mark("drop install");
                let took = t.elapsed();
                let (user2, sys2, major2, minor2) = usage();
                let steps: Vec<String> = (phases.steps.iter())
                    .map(|(s, d)| format!("{s} {:.1}", d.as_secs_f64() * 1e3))
                    .collect();
                let line = format!(
                    "{:.1} ms ({:.0} + {:.0} ms user + system CPU, {} major + {} minor faults): {}",
                    took.as_secs_f64() * 1e3,
                    (user2 - user).as_secs_f64() * 1e3,
                    (sys2 - sys).as_secs_f64() * 1e3,
                    major2 - major,
                    minor2 - minor,
                    steps.join(", ")
                );
                if best.as_ref().is_none_or(|b| took < b.0) {
                    best = Some((took, line));
                }
            }
            eprintln!("{var}: {}", best.unwrap().1);
        }
    }

    #[test]
    fn real_install_index() {
        let Some(dir) = crate::vfs::install_dir("ANNO117_DIR") else {
            return;
        };
        let t = Instant::now();
        let i = Install::open(&dir, Install::DATA_FIRST).unwrap();
        let took = t.elapsed();
        eprintln!(
            "indexed {} archives, {} visible files, {} shadowed, {} deleted, {} folders in {took:?}",
            i.archives().len(),
            i.file_count(),
            i.shadowed_count(),
            i.deleted_count(),
            i.folders.len(),
        );
        eprintln!(
            "memory: names {} B, folder paths {} B, {} layers x {} B, {} folders",
            i.names.len(),
            i.folder_paths.len(),
            i.layers.len(),
            std::mem::size_of::<Layer>(),
            i.folders.len()
        );
        // Independent count from rda.py: one line per (archive, entry).
        let inventory = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rda_inventory.tsv"
        ));
        if let Ok(inventory) = inventory {
            let mut names: Vec<_> = inventory
                .lines()
                .filter_map(|l| l.split('\t').next())
                .collect();
            let entries = names.len();
            names.dedup();
            assert_eq!(i.archives(), names);
            // rda.py lists live entries only. A path deleted and later
            // added again keeps its tombstone among its layers, counted as
            // shadowed: zz_patchfiles_300 brings back 411 files
            // zz_patchfiles_200 deleted.
            let reborn = i.layers.iter().filter(|l| l.is_tombstone()).count();
            assert_eq!(
                i.file_count() + i.shadowed_count() + i.deleted_count() - reborn,
                entries
            );
        }
        // 43 with DLC 3 (dlc03_graphics and dlc03_provinces in two parts
        // each) and patches 300 and 301.
        assert!(i.archives().len() >= 43, "{} archives", i.archives().len());

        let first = i.archives().iter().position(|a| a == "config.rda").unwrap() as ArchiveId;
        let cfg = i.archive(first).name().to_string();
        assert!(cfg.ends_with("config.rda"));
        let hits = i.search("data/ui/studio/generated/", 3);
        assert_eq!(hits.len(), 3);
        for id in hits {
            let w = i.winner(id);
            assert_eq!(i.read_file(id).unwrap().len() as u64, u64::from(w.raw_size));
        }
        assert!(t.elapsed().as_secs() < 30);
    }
}
