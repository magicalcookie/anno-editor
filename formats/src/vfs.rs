// The layered install view: every `maindata/*.rda` mounted in engine order,
// one winning layer per virtual path, a folder tree and search for the asset
// browser. Mount rules: ../PATCHING.md.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail, ensure};

use crate::game::{ArchiveOrder, Game};
use crate::rda::{Archive, BlockFlags, Entry};

pub(crate) mod discovery;
pub use discovery::steam_roots;

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
/// last one wins and is never a tombstone for an indexed file.
#[derive(Debug, Clone, Copy)]
pub struct Layer {
    pub archive: ArchiveId,
    pub flags: BlockFlags,
    pub offset: u64,
    pub compressed_size: u64,
    pub raw_size: u64,
    pub timestamp: u64,
    memres: u32,
}

impl Layer {
    /// This archive deletes the path (from earlier archives).
    pub fn is_tombstone(&self) -> bool {
        self.flags.deleted()
    }
}

#[derive(Debug, Clone, Copy)]
struct Folder {
    // The folder path is a prefix of some file path in `paths`.
    start: u32,
    len: u32,
    parent: u32,
    subfolders: (u32, u32),
    files: (u32, u32),
}

/// A mounted install. Opening reads only archive headers and header tables.
///
/// Memory: one `\0`-separated arena of normalized paths (~27 MB for the
/// shipped 292k paths), one 48-byte [`Layer`] per (path, archive) pair and
/// small per-folder records. Archive entry lists are dropped after indexing.
pub struct Install {
    game: Game,
    dir: PathBuf,
    archives: Vec<Archive>,
    archive_names: Vec<String>,
    // Sorted normalized paths, each followed by \0. File i spans
    // starts[i]..starts[i+1]-1.
    paths: String,
    starts: Vec<u32>,
    file_folder: Vec<u32>,
    layer_starts: Vec<u32>,
    layers: Vec<Layer>,
    folders: Vec<Folder>,
    folder_children: Vec<FolderId>,
    folder_files: Vec<FileId>,
    shadowed: usize,
    deleted: usize,
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

impl Install {
    /// The 117 environment override, else the first launcher installation.
    pub fn locate() -> Option<PathBuf> {
        Game::Anno117.locate()
    }

    /// Mounts `game_dir/maindata/*.rda` with the detected game's rules.
    /// Synthetic installs without executables retain the original 117 rules.
    pub fn open(game_dir: impl AsRef<Path>) -> Result<Install> {
        let dir = game_dir.as_ref().to_path_buf();
        let game = Game::detect(&dir).unwrap_or(Game::Anno117);
        Self::open_game(game, dir)
    }

    /// Opens with explicit game rules, including synthetic installs in tests.
    pub fn open_game(game: Game, game_dir: impl AsRef<Path>) -> Result<Install> {
        let dir = game_dir.as_ref().to_path_buf();
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
            if is_rda && item.file_type()?.is_file() {
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
            let number = match game.profile().archive_order {
                ArchiveOrder::Name => None,
                ArchiveOrder::Numbered => name
                    .strip_prefix("data")
                    .and_then(|n| n.strip_suffix(".rda"))
                    .and_then(|n| n.parse::<u32>().ok()),
            };
            (number.is_none(), number.unwrap_or(0), name)
        });
        Install::mount(game, dir, files)
    }

    pub fn game(&self) -> Game {
        self.game
    }

    fn mount(game: Game, dir: PathBuf, files: Vec<PathBuf>) -> Result<Install> {
        let archive_names = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        // Header tables of the big archives are tens of MB each; parse them
        // on all cores.
        let mut slots: Vec<Option<Result<Archive>>> = files.iter().map(|_| None).collect();
        let next = AtomicUsize::new(0);
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..workers.min(files.len()))
                .map(|_| {
                    s.spawn(|| {
                        let mut done = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(path) = files.get(i) else { break };
                            done.push((i, Archive::open(path)));
                        }
                        done
                    })
                })
                .collect();
            for h in handles {
                for (i, r) in h.join().expect("archive worker panicked") {
                    slots[i] = Some(r);
                }
            }
        });
        let mut archives = Vec::with_capacity(files.len());
        for slot in slots {
            archives.push(slot.expect("every archive opened")?);
        }
        let mut lists: Vec<(Vec<Entry>, Vec<Entry>)> =
            archives.iter_mut().map(Archive::take_index).collect();

        // (key, archive, entry); within one archive keys are already unique.
        let mut all: Vec<(Cow<str>, ArchiveId, &Entry)> = Vec::new();
        let total: usize = lists.iter().map(|(e, t)| e.len() + t.len()).sum();
        all.reserve(total);
        for (a, (entries, tombs)) in lists.iter_mut().enumerate() {
            for e in entries.iter().chain(tombs.iter()) {
                all.push((e.key(), a as ArchiveId, e));
            }
        }
        all.sort_unstable_by(|x, y| x.0.cmp(&y.0).then(x.1.cmp(&y.1)));

        let mut paths = String::new();
        let mut starts = Vec::new();
        let mut layer_starts = Vec::new();
        let mut layers = Vec::new();
        let mut shadowed = 0;
        let mut deleted = 0;
        for group in all.chunk_by(|x, y| x.0 == y.0) {
            let (key, _, last) = group.last().unwrap();
            if last.flags.deleted() {
                deleted += group.iter().filter(|g| !g.2.flags.deleted()).count();
                continue;
            }
            shadowed += group.len() - 1;
            ensure!(
                key.len() < u32::MAX as usize - paths.len(),
                "path arena over 4 GiB"
            );
            starts.push(paths.len() as u32);
            paths.push_str(key);
            paths.push('\0');
            layer_starts.push(layers.len() as u32);
            layers.extend(group.iter().map(|&(_, archive, e)| Layer {
                archive,
                flags: e.flags,
                offset: e.offset,
                compressed_size: e.compressed_size,
                raw_size: e.raw_size,
                timestamp: e.timestamp,
                memres: e.memres,
            }));
        }
        starts.push(paths.len() as u32);
        layer_starts.push(layers.len() as u32);
        drop(all);
        drop(lists);
        paths.shrink_to_fit();
        layers.shrink_to_fit();

        let mut install = Install {
            game,
            dir,
            archives,
            archive_names,
            paths,
            starts,
            file_folder: Vec::new(),
            layer_starts,
            layers,
            folders: Vec::new(),
            folder_children: Vec::new(),
            folder_files: Vec::new(),
            shadowed,
            deleted,
            overlay: RwLock::default(),
        };
        install.build_tree();
        Ok(install)
    }

    // Paths are sorted, so every folder's subtree is a contiguous run and one
    // stack of open folders is enough to assign each file.
    fn build_tree(&mut self) {
        let mut folders = vec![Folder {
            start: 0,
            len: 0,
            parent: u32::MAX,
            subfolders: (0, 0),
            files: (0, 0),
        }];
        let mut subfolders: Vec<Vec<FolderId>> = vec![Vec::new()];
        let mut files: Vec<Vec<FileId>> = vec![Vec::new()];
        let mut stack: Vec<u32> = vec![0];
        let mut file_folder = Vec::with_capacity(self.file_count());
        for i in 0..self.file_count() {
            let start = self.starts[i];
            let path = self.file_path(FileId(i as u32));
            let dir = &path[..path.rfind('/').unwrap_or(0)];
            while let Some(&top) = stack.last() {
                let f = folders[top as usize];
                let fpath = &self.paths[f.start as usize..][..f.len as usize];
                let ancestor = f.len == 0
                    || dir == fpath
                    || (dir.starts_with(fpath) && dir.as_bytes()[fpath.len()] == b'/');
                if ancestor {
                    break;
                }
                stack.pop();
            }
            let mut len = folders[*stack.last().unwrap() as usize].len as usize;
            while len < dir.len() {
                let from = if len == 0 { 0 } else { len + 1 };
                let end = dir[from..].find('/').map_or(dir.len(), |p| from + p);
                let parent = *stack.last().unwrap();
                let id = folders.len() as u32;
                folders.push(Folder {
                    start,
                    len: end as u32,
                    parent,
                    subfolders: (0, 0),
                    files: (0, 0),
                });
                subfolders.push(Vec::new());
                files.push(Vec::new());
                subfolders[parent as usize].push(FolderId(id));
                stack.push(id);
                len = end;
            }
            let top = *stack.last().unwrap();
            files[top as usize].push(FileId(i as u32));
            file_folder.push(top);
        }

        let name = |f: &Folder| {
            let p = &self.paths[f.start as usize..][..f.len as usize];
            &p[p.rfind('/').map_or(0, |i| i + 1)..]
        };
        let mut flat_children = Vec::with_capacity(folders.len());
        let mut flat_files = Vec::with_capacity(self.file_count());
        for (id, (mut subs, own)) in subfolders.into_iter().zip(files).enumerate() {
            // Path order puts "b-c/x" before "b/x"; the browser wants names.
            subs.sort_unstable_by(|&a, &b| {
                name(&folders[a.0 as usize]).cmp(name(&folders[b.0 as usize]))
            });
            let c0 = flat_children.len() as u32;
            flat_children.extend_from_slice(&subs);
            let f0 = flat_files.len() as u32;
            flat_files.extend_from_slice(&own);
            folders[id].subfolders = (c0, flat_children.len() as u32);
            folders[id].files = (f0, flat_files.len() as u32);
        }
        self.folders = folders;
        self.folder_children = flat_children;
        self.folder_files = flat_files;
        self.file_folder = file_folder;
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Mounted archive file names in mount order; index with [`ArchiveId`].
    pub fn archives(&self) -> &[String] {
        &self.archive_names
    }

    pub fn archive(&self, id: ArchiveId) -> &Archive {
        &self.archives[id as usize]
    }

    /// Visible virtual paths.
    pub fn file_count(&self) -> usize {
        self.starts.len() - 1
    }

    /// Entries hidden by a later archive's copy of the same path.
    pub fn shadowed_count(&self) -> usize {
        self.shadowed
    }

    /// Live entries whose path a later archive tombstoned.
    pub fn deleted_count(&self) -> usize {
        self.deleted
    }

    /// Normalized path (lowercase, `/`).
    pub fn file_path(&self, id: FileId) -> &str {
        let i = id.0 as usize;
        &self.paths[self.starts[i] as usize..self.starts[i + 1] as usize - 1]
    }

    pub fn file_name(&self, id: FileId) -> &str {
        let p = self.file_path(id);
        &p[p.rfind('/').map_or(0, |i| i + 1)..]
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
        let key = crate::rda::normalize_path(path);
        let n = self.file_count();
        let i = partition(n, |i| self.file_path(FileId(i as u32)) < key.as_ref());
        (i < n && self.file_path(FileId(i as u32)) == key).then_some(FileId(i as u32))
    }

    /// Serves the loose files of a mod folder (`<mod>/data/...`) over the
    /// archives, replacing the previous one. XML is left out: the game
    /// applies a mod's XML as ModOps patches, not as a replacement. Only
    /// [`Install::read`], [`Install::exists`] and [`Install::list`] see these
    /// files; the browser tree and searches stay the shipped index.
    pub fn mount_mod(&self, dir: &Path) -> Result<usize> {
        self.mount_mods(&[dir])
    }

    /// Serves several mod folders as [`Install::mount_mod`] does, later ones
    /// winning: the project's dependencies first, the project last. Each
    /// file remembers which folder it came from ([`Install::mod_layer`]).
    pub fn mount_mods(&self, dirs: &[&Path]) -> Result<usize> {
        let mut files = BTreeMap::new();
        for (layer, dir) in dirs.iter().enumerate() {
            let layer = u16::try_from(layer).context("too many mod folders")?;
            // Only `data/` is mounted; walking the rest would also walk a
            // mod's `.git` or nested mods.
            let mut stack = vec![dir.join("data")];
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
                    if rel.ends_with(".xml") {
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
                    .map(|&id| self.file_path(id).to_owned())
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
    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
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
            compressed_size: layer.compressed_size,
            raw_size: layer.raw_size,
            timestamp: layer.timestamp,
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
        let i = partition(n, |i| self.file_path(FileId(i as u32)) < prefix.as_str());
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
        &self.paths[f.start as usize..][..f.len as usize]
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
        // One pass over the arena; \0 separators keep matches inside a path.
        for (at, _) in self.paths.match_indices(query.as_str()) {
            let i = partition(self.file_count(), |i| self.starts[i + 1] as usize <= at);
            let id = FileId(i as u32);
            if hits.last() != Some(&id) {
                hits.push(id);
                if hits.len() == limit {
                    break;
                }
            }
        }
        hits
    }

    /// Where a mod archive called `name` goes so it mounts after every
    /// shipped patch. Fails if something already mounted would still sort
    /// after it, which means it would lose those conflicts.
    pub fn mod_archive_path(&self, name: &str) -> Result<PathBuf> {
        let file = mod_archive_name(name)?;
        let later: Vec<_> = self
            .archive_names
            .iter()
            .filter(|a| a.as_bytes() > file.as_bytes() && is_shipped_patch(a))
            .collect();
        ensure!(later.is_empty(), "{file} would mount before {later:?}");
        Ok(self.dir.join("maindata").join(file))
    }
}

/// `zz_patchfiles_999_<name>.rda`, the game's own patch scheme. ASCII `_`
/// sorts after digits, so this beats `zz_patchfiles_210.rda` and any future
/// three- or four-digit patch below 999. `name` is lowercased and limited to
/// `[a-z0-9_-]`.
pub fn mod_archive_name(name: &str) -> Result<String> {
    let name = name.trim().to_ascii_lowercase();
    ensure!(!name.is_empty(), "empty mod name");
    ensure!(
        name.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "mod name {name:?}: use only letters, digits, `_` and `-`"
    );
    Ok(format!("zz_patchfiles_999_{name}.rda"))
}

// zz_patchfiles_<digits>.rda, i.e. not another mod.
fn is_shipped_patch(file: &str) -> bool {
    let Some(rest) = file.strip_prefix("zz_patchfiles_") else {
        return false;
    };
    let digits = rest.strip_suffix(".rda").unwrap_or(rest);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
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

#[cfg(test)]
mod tests {
    use super::discovery::library_paths;
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
        p.write(&dir.join("maindata").join(name)).unwrap();
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
        let install = Install::open(d).unwrap();
        (dir, install)
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
        let (dep, project) = (mods.path().join("dep"), mods.path().join("project"));
        // Paths, not files: both folders have rock.cfg.
        assert_eq!(install.mount_mods(&[&dep, &project]).unwrap(), 2);
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
        install.mount_mod(&project).unwrap();
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
    fn mod_names_sort_after_shipped_patches() {
        let (_dir, i) = synthetic();
        let p = i.mod_archive_path("My-Mod").unwrap();
        assert!(p.ends_with("maindata/zz_patchfiles_999_my-mod.rda"));
        let name = mod_archive_name("x").unwrap();
        for shipped in [
            "zz_patchfiles_210.rda",
            "zz_patchfiles_999.rda",
            "zz_patchfiles_1000.rda",
        ] {
            assert!(name.as_bytes() > shipped.as_bytes(), "{shipped}");
        }
        assert!(mod_archive_name("a b").is_err());
        assert!(mod_archive_name("../x").is_err());
    }

    #[test]
    fn steam_library_vdf() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/home/a/.local/share/Steam\"\n\
                   \t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\Games\\\\Steam\"\n\t}\n}\n";
        assert_eq!(
            library_paths(vdf),
            [
                PathBuf::from("/home/a/.local/share/Steam"),
                PathBuf::from("D:\\Games\\Steam")
            ]
        );
    }

    #[test]
    fn steam_library_vdf_accepts_compact_metadata_and_comments() {
        let text = r#"// Steam library folders
            "libraryfolders" { "1" { "path" "D:\\Games\\Steam" } } // another drive
        "#;
        assert_eq!(library_paths(text), [PathBuf::from(r"D:\Games\Steam")]);
    }

    #[test]
    fn executable_and_empty_maindata_are_not_an_install() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Bin/Win64")).unwrap();
        std::fs::create_dir(dir.path().join("maindata")).unwrap();
        std::fs::write(dir.path().join("Bin/Win64/Anno1800.exe"), []).unwrap();
        assert_eq!(Game::detect(dir.path()), None);
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
        let install = Install::open_game(Game::Anno1800, dir.path()).unwrap();
        assert_eq!(install.game(), Game::Anno1800);
        assert_eq!(install.archives(), ["data2.rda", "data9.rda", "data10.rda"]);
        assert_eq!(install.read("data/shared.txt").unwrap(), b"ten");
        assert!(install.file("data/deleted.txt").is_none());
        let id = install.file("data/shared.txt").unwrap();
        assert_eq!(
            install.read_layer(id, &install.layers(id)[0]).unwrap(),
            b"two"
        );
        // The same filenames in a 117 install must still sort bytewise.
        let install = Install::open_game(Game::Anno117, dir.path()).unwrap();
        assert_eq!(install.read("data/shared.txt").unwrap(), b"nine");
    }

    #[test]
    fn game_detection_uses_executable_and_keeps_storage_separate() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Game::detect(dir.path()), None);
        std::fs::create_dir_all(dir.path().join("Bin/Win64")).unwrap();
        std::fs::create_dir(dir.path().join("maindata")).unwrap();
        pack(
            dir.path(),
            "data0.rda",
            &[("data/fixture.txt", b"fixture")],
            &[],
        );
        for (game, exe) in [
            (Game::Anno1800, "Anno1800.exe"),
            (Game::Anno117, "Anno117.exe"),
        ] {
            let exe = dir.path().join("Bin/Win64").join(exe);
            std::fs::write(&exe, []).unwrap();
            assert_eq!(Game::detect(dir.path()), Some(game));
            std::fs::remove_file(exe).unwrap();
        }
        assert_eq!(Game::Anno117.profile().config_folder, "anno-editor");
        assert_ne!(
            Game::Anno117.profile().config_folder,
            Game::Anno1800.profile().config_folder
        );
    }

    #[test]
    fn installed_1800_browses_and_reads_real_assets() {
        let Some(dir) = std::env::var_os("ANNO1800_DIR").map(PathBuf::from) else {
            return;
        };
        assert_eq!(Game::detect(&dir), Some(Game::Anno1800));
        let install = Install::open(&dir).unwrap();
        assert_eq!(install.game(), Game::Anno1800);
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
        let archive =
            Archive::from_bytes(install.read_file(island).unwrap(), "1800 island").unwrap();
        assert!(!archive.read_path("gamedata.data").unwrap().is_empty());
        assert!(!archive.read_path("rd3d.data").unwrap().is_empty());
    }

    #[test]
    fn real_install_index() {
        let Some(dir) = std::env::var_os("ANNO117_DIR").map(PathBuf::from) else {
            return;
        };
        assert_eq!(Install::locate().as_deref(), Some(dir.as_path()));
        let t = Instant::now();
        let i = Install::open(&dir).unwrap();
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
            "memory: path arena {} B, {} layers x {} B, {} folders",
            i.paths.len(),
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
            assert_eq!(
                i.file_count() + i.shadowed_count() + i.deleted_count(),
                entries
            );
        }
        assert_eq!(i.archives().len(), 37);

        let first = i.archives().iter().position(|a| a == "config.rda").unwrap() as ArchiveId;
        let cfg = i.archive(first).name().to_string();
        assert!(cfg.ends_with("config.rda"));
        let hits = i.search("data/ui/studio/generated/", 3);
        assert_eq!(hits.len(), 3);
        for id in hits {
            let w = i.winner(id);
            assert_eq!(i.read_file(id).unwrap().len() as u64, w.raw_size);
        }
        assert!(t.elapsed().as_secs() < 30);
    }
}
