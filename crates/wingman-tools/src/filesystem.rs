//! The filesystem seam.
//!
//! Every tool that touches a file went straight to `std::fs` or `tokio::fs`.
//! [`FileSystem`] puts one interface in front of that, with two
//! implementations: [`OsFileSystem`], which is the real thing, and
//! [`MemoryFileSystem`], which is a map.
//!
//! ## Scope — read this before assuming what swapping the backend does
//!
//! The seam covers **a tool's file content I/O**: reading, writing, listing,
//! metadata, removal. That boundary is exact and enforced by a test
//! (`no_tool_bypasses_the_filesystem_seam`) which fails if any tool reaches
//! for `std::fs` or `tokio::fs` directly. A seam half the tools honour is
//! worse than none, because swapping the backend would then silently work for
//! some of them.
//!
//! Two things are deliberately **outside** it:
//!
//! - **Path containment** (`ToolCtx::allows_read`, `is_inside_project`, and
//!   the canonicalization behind them). That is a security policy about real
//!   paths — whether a path escapes the project, whether a symlink points
//!   somewhere it should not — and it should consult the real filesystem
//!   whatever a tool happens to be reading through. Routing it through a
//!   swappable backend would mean a backend could answer "yes, that is inside
//!   the project" about a path that is not.
//! - **The audit log** in the registry. Infrastructure, not a tool, and a
//!   compliance trail that a swapped backend could redirect is not a
//!   compliance trail.
//!
//! ## Why async
//!
//! Tools are `async fn run`, and the file tools already used `tokio::fs`.
//! A sync trait would have forced blocking I/O back into the async loop.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;

/// What a tool needs to know about a directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub len: u64,
}

/// What a tool needs to know about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub is_dir: bool,
    pub len: u64,
    /// When the file was last written, where the backend knows.
    ///
    /// `recall_memory` cites a memory's age as provenance, so this
    /// is part of what a tool needs rather than filesystem trivia.
    pub modified: Option<std::time::SystemTime>,
}

/// The file operations tools perform.
#[async_trait]
pub trait FileSystem: Send + Sync {
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
    async fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    async fn remove_file(&self, path: &Path) -> io::Result<()>;
    async fn metadata(&self, path: &Path) -> io::Result<Meta>;
    /// Entries directly inside `path`, in no guaranteed order.
    async fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;

    /// Read a file as UTF-8.
    ///
    /// Provided rather than required: it is `read` plus a decode, and an
    /// implementation that got the two out of step would be a bug with no
    /// upside.
    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let bytes = self.read(path).await?;
        String::from_utf8(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not valid UTF-8"))
    }

    /// Whether anything exists at `path`.
    async fn exists(&self, path: &Path) -> bool {
        self.metadata(path).await.is_ok()
    }

    /// Read a file from a blocking context.
    ///
    /// `grep`, `find_symbol` and `who_calls` walk the tree inside
    /// `tokio::task::spawn_blocking` and read each candidate as they go —
    /// deliberately, because that is a CPU- and IO-bound scan that has no
    /// business on the async runtime. There is no `.await` available in there.
    ///
    /// The alternatives were worse than a second method: `block_on` inside a
    /// blocking closure deadlocks on a current-thread runtime, and
    /// restructuring the walk to collect paths first and read them
    /// asynchronously would move thousands of small reads onto the async
    /// runtime to satisfy the shape of an interface. Reads genuinely happen in
    /// two contexts, so the trait says so.
    fn read_blocking(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// [`read_blocking`](Self::read_blocking) as UTF-8.
    fn read_to_string_blocking(&self, path: &Path) -> io::Result<String> {
        let bytes = self.read_blocking(path)?;
        String::from_utf8(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not valid UTF-8"))
    }
}

/// The real filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsFileSystem;

#[async_trait]
impl FileSystem for OsFileSystem {
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        tokio::fs::read(path).await
    }
    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        tokio::fs::write(path, contents).await
    }
    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        tokio::fs::create_dir_all(path).await
    }
    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        tokio::fs::remove_file(path).await
    }
    async fn metadata(&self, path: &Path) -> io::Result<Meta> {
        let m = tokio::fs::metadata(path).await?;
        Ok(Meta {
            is_dir: m.is_dir(),
            len: m.len(),
            modified: m.modified().ok(),
        })
    }
    fn read_blocking(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
    async fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let mut rd = tokio::fs::read_dir(path).await?;
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            let meta = entry.metadata().await?;
            out.push(DirEntry {
                path: entry.path(),
                is_dir: meta.is_dir(),
                len: meta.len(),
            });
        }
        Ok(out)
    }
}

/// One in-memory file: its bytes and when they were last written.
#[derive(Debug, Clone)]
struct MemFile {
    bytes: Vec<u8>,
    written: std::time::SystemTime,
}

/// A filesystem in a map, for tests.
///
/// Directories are implicit: a file at `a/b/c.txt` makes `a` and `a/b` exist
/// as directories. `create_dir_all` therefore records them explicitly only so
/// that an empty directory can exist at all.
#[derive(Debug, Default)]
pub struct MemoryFileSystem {
    files: Mutex<HashMap<PathBuf, MemFile>>,
    dirs: Mutex<Vec<PathBuf>>,
}

impl MemoryFileSystem {
    pub fn new() -> Self {
        Self::default()
    }

    /// Put a file there without going through the trait, for setting a test up.
    pub fn seed(&self, path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
        self.files.lock().unwrap().insert(
            path.as_ref().to_path_buf(),
            MemFile {
                bytes: contents.as_ref().to_vec(),
                written: std::time::SystemTime::now(),
            },
        );
    }

    fn is_implicit_dir(&self, path: &Path) -> bool {
        self.dirs.lock().unwrap().iter().any(|d| d == path)
            || self
                .files
                .lock()
                .unwrap()
                .keys()
                .any(|f| f.parent().is_some_and(|p| p.starts_with(path)) || f.starts_with(path))
    }
}

#[async_trait]
impl FileSystem for MemoryFileSystem {
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .map(|f| f.bytes.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{}", path.display())))
    }

    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        self.files.lock().unwrap().insert(
            path.to_path_buf(),
            MemFile {
                bytes: contents.to_vec(),
                written: std::time::SystemTime::now(),
            },
        );
        Ok(())
    }

    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let mut dirs = self.dirs.lock().unwrap();
        if !dirs.iter().any(|d| d == path) {
            dirs.push(path.to_path_buf());
        }
        Ok(())
    }

    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.files
            .lock()
            .unwrap()
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{}", path.display())))
    }

    async fn metadata(&self, path: &Path) -> io::Result<Meta> {
        if let Some(entry) = self.files.lock().unwrap().get(path) {
            return Ok(Meta {
                is_dir: false,
                len: entry.bytes.len() as u64,
                modified: Some(entry.written),
            });
        }
        if self.is_implicit_dir(path) {
            return Ok(Meta {
                is_dir: true,
                len: 0,
                modified: None,
            });
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{}", path.display()),
        ))
    }

    fn read_blocking(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .map(|f| f.bytes.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{}", path.display())))
    }

    async fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let files = self.files.lock().unwrap();
        let mut out: Vec<DirEntry> = Vec::new();
        for (p, f) in files.iter() {
            if p.parent() == Some(path) {
                out.push(DirEntry {
                    path: p.clone(),
                    is_dir: false,
                    len: f.bytes.len() as u64,
                });
            }
        }
        for d in self.dirs.lock().unwrap().iter() {
            if d.parent() == Some(path) {
                out.push(DirEntry {
                    path: d.clone(),
                    is_dir: true,
                    len: 0,
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One script, both implementations — a seam whose implementations
    /// disagree is worse than no seam, because callers are written against
    /// whichever one they happened to test with.
    async fn behaves_like_a_filesystem(fs: &dyn FileSystem, root: &Path) {
        let file = root.join("a.txt");
        assert!(!fs.exists(&file).await);
        assert!(fs.read(&file).await.is_err(), "missing file must error");

        fs.create_dir_all(root).await.unwrap();
        fs.write(&file, b"hello").await.unwrap();

        assert!(fs.exists(&file).await);
        assert_eq!(fs.read(&file).await.unwrap(), b"hello");
        assert_eq!(fs.read_to_string(&file).await.unwrap(), "hello");

        let meta = fs.metadata(&file).await.unwrap();
        assert!(!meta.is_dir);
        assert_eq!(meta.len, 5);

        // Overwrite.
        fs.write(&file, b"bye").await.unwrap();
        assert_eq!(fs.read_to_string(&file).await.unwrap(), "bye");

        // The blocking flavour must agree with the async one.
        assert_eq!(fs.read_blocking(&file).unwrap(), b"bye");
        assert_eq!(fs.read_to_string_blocking(&file).unwrap(), "bye");

        let entries = fs.read_dir(root).await.unwrap();
        assert!(entries.iter().any(|e| e.path == file && !e.is_dir));

        fs.remove_file(&file).await.unwrap();
        assert!(!fs.exists(&file).await);
        assert!(fs.remove_file(&file).await.is_err(), "double remove errors");
    }

    #[tokio::test]
    async fn the_memory_filesystem_behaves_like_a_filesystem() {
        let fs = MemoryFileSystem::new();
        behaves_like_a_filesystem(&fs, Path::new("/mem/root")).await;
    }

    #[tokio::test]
    async fn the_os_filesystem_behaves_like_a_filesystem() {
        let root = std::env::temp_dir().join(format!(
            "wingman-fs-seam-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        behaves_like_a_filesystem(&OsFileSystem, &root).await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn non_utf8_content_is_an_error_not_a_mangled_string() {
        let fs = MemoryFileSystem::new();
        fs.seed("/x.bin", [0xff, 0xfe, 0x00]);
        assert!(fs.read(Path::new("/x.bin")).await.is_ok(), "bytes are fine");
        assert!(
            fs.read_to_string(Path::new("/x.bin")).await.is_err(),
            "a binary file must not decode into replacement characters"
        );
    }

    #[tokio::test]
    async fn seeding_sets_a_file_up_without_going_through_write() {
        let fs = MemoryFileSystem::new();
        fs.seed("/seeded.txt", "content");
        assert_eq!(
            fs.read_to_string(Path::new("/seeded.txt")).await.unwrap(),
            "content"
        );
    }
}

#[cfg(test)]
mod boundary_tests {
    /// No tool may reach the filesystem directly.
    ///
    /// This is what makes the seam mean something. A seam half the tools
    /// honour is worse than none: swapping the backend would silently work for
    /// some of them and not others, and nothing would say so. The rule is
    /// therefore enforced rather than documented and hoped for.
    ///
    /// Scope is deliberate and stated in the module docs — containment
    /// canonicalization and the audit log live outside `builtin/` precisely
    /// because they should keep consulting the real filesystem.
    #[test]
    fn no_tool_bypasses_the_filesystem_seam() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/builtin");
        let mut offenders: Vec<String> = Vec::new();

        for entry in std::fs::read_dir(&dir).expect("builtin dir") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read source");
            // Tests set fixtures up on the real filesystem, which is fine and
            // expected — only production code is bound by the rule.
            let production = match source.find("mod tests") {
                Some(i) => &source[..i],
                None => &source[..],
            };
            for (n, line) in production.lines().enumerate() {
                if line.contains("std::fs::") || line.contains("tokio::fs::") {
                    offenders.push(format!(
                        "{}:{}: {}",
                        path.file_name().unwrap().to_string_lossy(),
                        n + 1,
                        line.trim()
                    ));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "these tools reach the filesystem directly instead of through \
             ToolCtx::fs, so swapping the backend would not affect them:\n  {}",
            offenders.join("\n  ")
        );
    }
}
