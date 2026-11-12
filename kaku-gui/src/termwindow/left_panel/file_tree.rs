//! The Files tool: a lazily read tree rooted at the active pane's project.
//!
//! The root follows the active pane's working directory, widened to the Git
//! work tree that contains it, until the user picks a root from the context
//! menu. Directories are read when expanded and re-read on `refresh`, which
//! the panel calls on a timer while it is visible, so files created by a
//! shell or an agent show up without a manual reload.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Entries never listed: VCS internals and Finder metadata.
const HIDDEN_NAMES: &[&str] = &[".git", ".DS_Store", ".hg", ".svn"];
/// A directory with more entries than this lists only the first ones.
const MAX_ENTRIES_PER_DIR: usize = 2000;
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// Matched by the repository's ignore rules; drawn dimmed.
    pub ignored: bool,
}

/// One visible line of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TreeRow {
    pub entry: Entry,
    pub depth: usize,
    pub expanded: bool,
}

#[derive(Default)]
pub(crate) struct FileTree {
    root: Option<PathBuf>,
    /// Set once the user chose a root; the active pane no longer moves it.
    pinned: bool,
    expanded: BTreeSet<PathBuf>,
    listings: HashMap<PathBuf, Vec<Entry>>,
    ignore: Option<ignore::gitignore::Gitignore>,
    last_refresh: Option<Instant>,
    /// The file the active viewer pane shows, highlighted in the tree.
    pub selected: Option<PathBuf>,
}

/// The Git work tree containing `dir`, found by walking up to a `.git`
/// entry (a directory, or a file in a linked worktree).
pub(crate) fn project_root(dir: &Path) -> PathBuf {
    let mut cur = Some(dir);
    while let Some(candidate) = cur {
        if candidate.join(".git").exists() {
            return candidate.to_path_buf();
        }
        cur = candidate.parent();
    }
    dir.to_path_buf()
}

fn build_ignore(root: &Path) -> Option<ignore::gitignore::Gitignore> {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    let mut any = false;
    for name in [".gitignore", ".ignore"] {
        let path = root.join(name);
        if path.is_file() {
            any |= builder.add(path).is_none();
        }
    }
    let exclude = root.join(".git").join("info").join("exclude");
    if exclude.is_file() {
        any |= builder.add(exclude).is_none();
    }
    if !any {
        return None;
    }
    builder.build().ok()
}

/// Lists `dir`: directories first, then files, each by case-insensitive
/// name, skipping VCS internals.
pub(crate) fn read_dir_sorted(
    dir: &Path,
    ignore: Option<&ignore::gitignore::Gitignore>,
) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut entries: Vec<Entry> = read
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if HIDDEN_NAMES.contains(&name.as_str()) {
                return None;
            }
            let path = entry.path();
            // Follow symlinks so a linked directory expands like a real one.
            let is_dir = std::fs::metadata(&path)
                .map(|meta| meta.is_dir())
                .unwrap_or(false);
            let ignored = ignore
                .map(|gi| gi.matched_path_or_any_parents(&path, is_dir).is_ignore())
                .unwrap_or(false);
            Some(Entry {
                name,
                path,
                is_dir,
                ignored,
            })
        })
        .take(MAX_ENTRIES_PER_DIR)
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    entries
}

impl FileTree {
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Follows the active pane's directory unless the user pinned a root.
    /// Returns true when the root changed.
    pub fn follow_cwd(&mut self, cwd: Option<&Path>) -> bool {
        if self.pinned {
            return false;
        }
        let Some(cwd) = cwd else {
            return false;
        };
        let root = project_root(cwd);
        if self.root.as_deref() == Some(root.as_path()) {
            return false;
        }
        self.set_root(root);
        true
    }

    pub fn pin_root(&mut self, root: PathBuf) {
        self.pinned = true;
        self.set_root(root);
    }

    fn set_root(&mut self, root: PathBuf) {
        self.expanded.clear();
        self.listings.clear();
        self.ignore = build_ignore(&root);
        self.expanded.insert(root.clone());
        self.root = Some(root);
        self.last_refresh = None;
        self.refresh();
    }

    pub fn toggle(&mut self, dir: &Path) {
        if !self.expanded.remove(dir) {
            self.expanded.insert(dir.to_path_buf());
            let listing = read_dir_sorted(dir, self.ignore.as_ref());
            self.listings.insert(dir.to_path_buf(), listing);
        }
    }

    /// Re-reads every expanded directory. Returns true when a listing
    /// changed. Directories that vanished collapse.
    pub fn refresh(&mut self) -> bool {
        self.last_refresh = Some(Instant::now());
        let mut changed = false;
        let expanded: Vec<PathBuf> = self.expanded.iter().cloned().collect();
        for dir in expanded {
            if !dir.is_dir() {
                self.expanded.remove(&dir);
                changed |= self.listings.remove(&dir).is_some();
                continue;
            }
            let listing = read_dir_sorted(&dir, self.ignore.as_ref());
            if self.listings.get(&dir) != Some(&listing) {
                self.listings.insert(dir, listing);
                changed = true;
            }
        }
        changed
    }

    /// `refresh`, at most once per interval.
    pub fn refresh_if_due(&mut self) -> bool {
        let due = self
            .last_refresh
            .map_or(true, |at| at.elapsed() >= REFRESH_INTERVAL);
        due && self.refresh()
    }

    /// The visible rows in display order: the root itself first, as a
    /// folder that collapses like any other, then its contents.
    pub fn rows(&self) -> Vec<TreeRow> {
        let mut out = vec![];
        if let Some(root) = &self.root {
            let expanded = self.expanded.contains(root);
            out.push(TreeRow {
                entry: Entry {
                    name: root
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| root.to_string_lossy().into_owned()),
                    path: root.clone(),
                    is_dir: true,
                    ignored: false,
                },
                depth: 0,
                expanded,
            });
            if expanded {
                self.push_rows(root, 1, &mut out);
            }
        }
        out
    }

    fn push_rows(&self, dir: &Path, depth: usize, out: &mut Vec<TreeRow>) {
        let Some(listing) = self.listings.get(dir) else {
            return;
        };
        for entry in listing {
            let expanded = entry.is_dir && self.expanded.contains(&entry.path);
            out.push(TreeRow {
                entry: entry.clone(),
                depth,
                expanded,
            });
            if expanded {
                self.push_rows(&entry.path, depth + 1, out);
            }
        }
    }

    /// Opens every ancestor of `path` below the root so it shows.
    pub fn reveal(&mut self, path: &Path) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let Ok(rel) = path.strip_prefix(&root) else {
            return;
        };
        let mut dir = root;
        let components: Vec<_> = rel.components().collect();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            dir = dir.join(component);
            if !self.expanded.contains(&dir) {
                self.toggle(&dir);
            }
        }
    }
}

/// `path` relative to `root` when it is inside it.
pub(crate) fn relative_to(path: &Path, root: Option<&Path>) -> String {
    root.and_then(|root| path.strip_prefix(root).ok())
        .map(|rel| rel.to_string_lossy().into_owned())
        .filter(|rel| !rel.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sumi-file-tree-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn listings_put_directories_first_and_hide_vcs_internals() {
        let dir = scratch("sort");
        std::fs::create_dir(dir.join("src")).unwrap();
        std::fs::create_dir(dir.join(".git")).unwrap();
        std::fs::write(dir.join("b.txt"), "").unwrap();
        std::fs::write(dir.join("A.md"), "").unwrap();
        std::fs::write(dir.join(".DS_Store"), "").unwrap();
        let names: Vec<_> = read_dir_sorted(&dir, None)
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["src", "A.md", "b.txt"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn root_widens_to_the_git_work_tree_and_marks_ignored_entries() {
        let dir = scratch("root");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("crate/src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        let mut tree = FileTree::default();
        assert!(tree.follow_cwd(Some(&dir.join("crate/src"))));
        assert_eq!(tree.root(), Some(dir.as_path()));
        let rows = tree.rows();
        let target = rows.iter().find(|r| r.entry.name == "target").unwrap();
        assert!(target.entry.ignored);
        let krate = rows.iter().find(|r| r.entry.name == "crate").unwrap();
        assert!(!krate.entry.ignored);
        // Same project: the root stays put.
        assert!(!tree.follow_cwd(Some(&dir.join("crate"))));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expanding_nests_rows_and_refresh_picks_up_new_files() {
        let dir = scratch("expand");
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("docs/a.md"), "").unwrap();
        let mut tree = FileTree::default();
        tree.pin_root(dir.clone());
        assert_eq!(tree.rows().len(), 2);
        tree.toggle(&dir.join("docs"));
        let rows = tree.rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            (rows[0].entry.path.as_path(), rows[0].depth),
            (dir.as_path(), 0)
        );
        assert_eq!((rows[2].entry.name.as_str(), rows[2].depth), ("a.md", 2));
        std::fs::write(dir.join("docs/b.md"), "").unwrap();
        assert!(tree.refresh());
        assert_eq!(tree.rows().len(), 4);
        assert!(!tree.refresh());
        // The root row collapses the whole tree and opens it again.
        tree.toggle(&dir);
        assert_eq!(tree.rows().len(), 1);
        assert!(!tree.rows()[0].expanded);
        tree.toggle(&dir);
        assert_eq!(tree.rows().len(), 4);
        // A pinned root ignores the active pane.
        assert!(!tree.follow_cwd(Some(Path::new("/"))));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reveal_expands_the_ancestors_of_a_file() {
        let dir = scratch("reveal");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/c.rs"), "").unwrap();
        let mut tree = FileTree::default();
        tree.pin_root(dir.clone());
        tree.reveal(&dir.join("a/b/c.rs"));
        assert!(tree.rows().iter().any(|r| r.entry.name == "c.rs"));
        assert_eq!(relative_to(&dir.join("a/b/c.rs"), Some(&dir)), "a/b/c.rs");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
