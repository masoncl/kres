//! Session-scoped read-consent store.
//!
//! Reads that escape kres's `--workspace DIR` are rejected by
//! `resolve_workspace` unless the requested path sits under a
//! directory the operator has named in a prompt during this
//! session. Intent: the operator explicitly mentioning `/etc/hosts`
//! or `/home/user/proj/` in their prompt is a clear enough signal
//! to auto-grant reads under the containing directory for the rest
//! of the session.
//!
//! No on-disk persistence and no slash commands — the only way to
//! grant is to mention a path in a prompt. Restart of kres starts
//! with a clean slate.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

/// Runtime consent store. Holds a set of directories the session has
/// granted read access to, plus the workspace root (which is always
/// allowed without needing to be named).
pub struct ConsentStore {
    granted: RwLock<BTreeSet<PathBuf>>,
}

impl ConsentStore {
    pub fn new() -> Self {
        Self {
            granted: RwLock::new(BTreeSet::new()),
        }
    }

    /// True when `candidate` sits under any granted directory.
    /// `candidate` is expected to be an absolute path (canonicalised
    /// where possible — see `resolve_workspace`).
    pub fn is_allowed(&self, candidate: &Path) -> bool {
        let g = self.granted.read().unwrap();
        g.iter().any(|dir| candidate.starts_with(dir))
    }

    /// Grant read access for a path the operator just mentioned.
    /// If the path is a file, the parent directory is granted (so
    /// naming one file in a project implies reading siblings too —
    /// that matches operator intent in practice). If the path is a
    /// directory, the directory itself is granted. Paths are
    /// canonicalised so `/proj/../proj` and symlinks collapse to a
    /// single entry.  Returns the directory that was ultimately
    /// added, or None if the path couldn't be resolved.
    pub fn grant_from_mention(&self, mentioned: &Path) -> Option<PathBuf> {
        let canon = mentioned.canonicalize().ok()?;
        let dir = if canon.is_dir() {
            canon
        } else if canon.is_file() {
            canon.parent()?.to_path_buf()
        } else {
            return None;
        };
        let mut g = self.granted.write().unwrap();
        g.insert(dir.clone());
        Some(dir)
    }

    /// Current set of granted directories (snapshot).
    pub fn list(&self) -> Vec<PathBuf> {
        self.granted.read().unwrap().iter().cloned().collect()
    }
}

impl Default for ConsentStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Global handle. `resolve_workspace` consults this when a requested
/// path escapes the workspace. The REPL installs the store at
/// startup; tool callers see `None` in unit tests, which falls back
/// to the pre-consent behaviour (hard reject on escape).
static GLOBAL: OnceLock<Arc<ConsentStore>> = OnceLock::new();

pub fn install(store: Arc<ConsentStore>) -> Result<(), Arc<ConsentStore>> {
    GLOBAL.set(store)
}

pub fn get() -> Option<Arc<ConsentStore>> {
    GLOBAL.get().cloned()
}

/// Scan a block of operator text for path-like tokens and grant
/// each one that resolves to an existing file or directory. Returns
/// the directories that were newly granted (deduped). The `cwd` is
/// used as the base for relative paths; absolute paths are used
/// verbatim.
///
/// A "path-like token" is a whitespace-separated chunk that starts
/// with `/`, `./`, `../`, or `~/`, OR contains a `/` and matches a
/// real filesystem entry when joined with cwd. Tokens are stripped
/// of common trailing punctuation (`,`, `.`, `:`, `;`, `!`, `?`,
/// backticks, parens, brackets, braces, quotes) before resolution —
/// operator prose tends to punctuate paths.
pub fn grant_paths_from_text(store: &ConsentStore, cwd: &Path, text: &str) -> Vec<PathBuf> {
    let mut added: Vec<PathBuf> = Vec::new();
    for raw in text.split_whitespace() {
        let candidate = strip_token_punctuation(raw);
        if candidate.is_empty() {
            continue;
        }
        if !looks_like_path(candidate) {
            continue;
        }
        let resolved = resolve_candidate(cwd, candidate);
        let Some(p) = resolved else {
            continue;
        };
        if let Some(dir) = store.grant_from_mention(&p) {
            if !added.contains(&dir) {
                added.push(dir);
            }
        }
    }
    added
}

fn strip_token_punctuation(s: &str) -> &str {
    let trimmed = s.trim_matches(|c: char| {
        matches!(
            c,
            ',' | '.'
                | ':'
                | ';'
                | '!'
                | '?'
                | '`'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '\''
                | '"'
                | '<'
                | '>'
        )
    });
    trimmed
}

fn looks_like_path(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with("~/")
        || s.contains('/')
}

fn resolve_candidate(cwd: &Path, s: &str) -> Option<PathBuf> {
    let expanded: PathBuf = if let Some(rest) = s.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else if s == "~" {
        dirs::home_dir()?
    } else {
        PathBuf::from(s)
    };
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    // metadata() follows symlinks, which is what we want — a
    // mention of a symlink grants the same target as a direct
    // mention would.
    if std::fs::metadata(&joined).is_ok() {
        Some(joined)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_store_denies_everything() {
        let s = ConsentStore::new();
        assert!(!s.is_allowed(Path::new("/etc")));
        assert!(!s.is_allowed(Path::new("/tmp/x")));
    }

    #[test]
    fn grant_from_mention_file_grants_parent() {
        let tmp = std::env::temp_dir();
        let file = tmp.join(format!("kres-consent-test-{}.txt", std::process::id()));
        std::fs::write(&file, b"").unwrap();
        let s = ConsentStore::new();
        let dir = s.grant_from_mention(&file).unwrap();
        assert_eq!(dir, file.parent().unwrap().canonicalize().unwrap());
        assert!(s.is_allowed(&file));
        assert!(s.is_allowed(&file.parent().unwrap().join("other.txt")));
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn grant_from_mention_dir_grants_that_dir() {
        let tmp = std::env::temp_dir();
        let d = tmp.join(format!("kres-consent-dir-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let s = ConsentStore::new();
        let got = s.grant_from_mention(&d).unwrap();
        let canon = d.canonicalize().unwrap();
        assert_eq!(got, canon);
        assert!(s.is_allowed(&canon.join("x")));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn grant_from_mention_missing_returns_none() {
        let s = ConsentStore::new();
        let got = s.grant_from_mention(Path::new("/definitely/does/not/exist/here"));
        assert!(got.is_none());
    }

    #[test]
    fn text_scanner_picks_up_absolute_path() {
        let tmp = std::env::temp_dir();
        let d = tmp.join(format!("kres-scan-dir-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let s = ConsentStore::new();
        let msg = format!("please read {} carefully.", d.display());
        let added = grant_paths_from_text(&s, Path::new("/tmp"), &msg);
        let canon = d.canonicalize().unwrap();
        assert!(added.contains(&canon));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn text_scanner_handles_trailing_punctuation() {
        let tmp = std::env::temp_dir();
        let d = tmp.join(format!("kres-scan-punct-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let s = ConsentStore::new();
        let msg = format!("check `{}`, thanks.", d.display());
        let added = grant_paths_from_text(&s, Path::new("/tmp"), &msg);
        let canon = d.canonicalize().unwrap();
        assert!(added.contains(&canon), "added={added:?}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn text_scanner_ignores_non_paths() {
        let s = ConsentStore::new();
        let added = grant_paths_from_text(&s, Path::new("/tmp"), "just some text no paths here");
        assert!(added.is_empty());
    }

    #[test]
    fn looks_like_path_filters_bare_identifiers() {
        assert!(!looks_like_path("hello"));
        assert!(!looks_like_path("hello.c"));
        assert!(looks_like_path("/etc/hosts"));
        assert!(looks_like_path("./foo"));
        assert!(looks_like_path("../foo"));
        assert!(looks_like_path("~/proj"));
        assert!(looks_like_path("fs/btrfs/ctree.c"));
    }
}
