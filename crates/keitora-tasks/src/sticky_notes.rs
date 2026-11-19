//! Per-worktree sticky notes: free-form notes a user leaves themselves about what is
//! going on in a worktree, shown over its terminal. They live in a JSON file next to
//! the project's task DB, so they never touch the task store's schema, the dispatch
//! flow, or the committed `keitora.toml`.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// File name of the notes store, beside the project's task DB.
pub const FILE_NAME: &str = "sticky-notes.json";

/// One worktree's note: its text, whether it is shown, and where it sits over the
/// terminal (pixels from the stage's top-left corner).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StickyNote {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub visible: bool,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
}

impl StickyNote {
    /// A hidden note with no text carries nothing worth keeping.
    fn is_blank(&self) -> bool {
        !self.visible && self.text.trim().is_empty()
    }
}

/// On-disk shape. Wrapped in an object so fields can be added later without
/// breaking older files.
#[derive(Debug, Default, Serialize, Deserialize)]
struct NotesFile {
    #[serde(default)]
    notes: BTreeMap<String, StickyNote>,
}

/// Serializes read-modify-write cycles within the process, so two quick saves for
/// different worktrees cannot drop each other's change.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Where a project's notes live: beside its task DB.
pub fn notes_path(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(FILE_NAME)
}

fn load_file(path: &Path) -> io::Result<NotesFile> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {e}", path.display()),
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(NotesFile::default()),
        Err(e) => Err(e),
    }
}

/// The note for `worktree`, or `None` when it has none (or the file does not exist).
pub fn get(path: &Path, worktree: &str) -> io::Result<Option<StickyNote>> {
    Ok(load_file(path)?.notes.remove(worktree))
}

/// Store `note` for `worktree`. A hidden note with no text is removed rather than
/// kept. Writes through a temp file + rename so a crash never leaves a torn file,
/// and refuses to overwrite a file it cannot parse.
pub fn put(path: &Path, worktree: &str, note: StickyNote) -> io::Result<()> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = load_file(path)?;
    if note.is_blank() {
        if file.notes.remove(worktree).is_none() {
            return Ok(());
        }
    } else {
        file.notes.insert(worktree.to_string(), note);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&file).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn note(text: &str, visible: bool) -> StickyNote {
        StickyNote {
            text: text.into(),
            visible,
            x: 12.0,
            y: 34.0,
        }
    }

    #[test]
    fn path_sits_beside_the_task_db() {
        let p = notes_path(Path::new("/data/proj/shared/tasks.db"));
        assert_eq!(p, Path::new("/data/proj/shared").join(FILE_NAME));
    }

    #[test]
    fn missing_file_reads_as_no_note() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(get(&tmp.path().join(FILE_NAME), "alpha").unwrap(), None);
    }

    #[test]
    fn put_then_get_round_trips_per_worktree() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("shared").join(FILE_NAME);
        put(&path, "alpha", note("rebase pending", true)).unwrap();
        put(&path, "bravo", note("waiting on review", false)).unwrap();

        assert_eq!(
            get(&path, "alpha").unwrap(),
            Some(note("rebase pending", true))
        );
        assert_eq!(
            get(&path, "bravo").unwrap(),
            Some(note("waiting on review", false))
        );
        assert_eq!(get(&path, "charlie").unwrap(), None);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn blank_hidden_note_is_removed() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(FILE_NAME);
        put(&path, "alpha", note("x", true)).unwrap();
        put(&path, "alpha", note("  ", false)).unwrap();
        assert_eq!(get(&path, "alpha").unwrap(), None);
        // Removing a note that is not there does not create the file.
        let other = tmp.path().join("other.json");
        put(&other, "alpha", StickyNote::default()).unwrap();
        assert!(!other.exists());
    }

    #[test]
    fn visible_empty_note_is_kept() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(FILE_NAME);
        put(&path, "alpha", note("", true)).unwrap();
        assert_eq!(get(&path, "alpha").unwrap(), Some(note("", true)));
    }

    #[test]
    fn corrupt_file_is_an_error_and_not_overwritten() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(FILE_NAME);
        std::fs::write(&path, "not json").unwrap();
        assert!(get(&path, "alpha").is_err());
        assert!(put(&path, "alpha", note("x", true)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn missing_fields_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(FILE_NAME);
        std::fs::write(&path, r#"{"notes":{"alpha":{"text":"hi"}}}"#).unwrap();
        assert_eq!(
            get(&path, "alpha").unwrap(),
            Some(StickyNote {
                text: "hi".into(),
                ..StickyNote::default()
            })
        );
    }
}
