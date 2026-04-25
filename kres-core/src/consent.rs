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
        // Granting the filesystem root would make is_allowed() true
        // for every absolute path on the host, which defeats the
        // entire consent gate. A bare `/` token is never a meaningful
        // operator mention; refuse to record it. (`looks_like_path`
        // also filters this token, but leaves the rule here as
        // belt-and-braces for any future caller that resolves a path
        // outside grant_paths_from_text.)
        if dir.parent().is_none() {
            return None;
        }
        let mut g = self.granted.write().unwrap();
        g.insert(dir.clone());
        Some(dir)
    }

    /// Current set of granted directories (snapshot).
    pub fn list(&self) -> Vec<PathBuf> {
        self.granted.read().unwrap().iter().cloned().collect()
    }

    /// Drop every grant. Used by `/clear` so a "start over" really
    /// starts over — including outside-workspace consents.
    pub fn clear(&self) -> usize {
        let mut g = self.granted.write().unwrap();
        let n = g.len();
        g.clear();
        n
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

/// One newly-granted directory plus a suspicion flag the caller
/// uses to print a louder warning when the grant covers a
/// top-level system tree (`/usr`, `/etc`, `/var`, `/opt`, the
/// operator's `$HOME`, …). Operators paste stack traces and
/// library paths into prompts; a single mention of
/// `/usr/lib/x86_64-linux-gnu/libc.so.6` would otherwise quietly
/// grant the parent dir without flagging it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantedPath {
    pub dir: PathBuf,
    pub suspicious: bool,
}

/// Scan a block of operator text for path-like tokens and grant
/// each one that resolves to an existing file or directory. Returns
/// the directories that were newly granted (deduped) wrapped in
/// `GrantedPath` so the caller can flag wide grants. The `cwd` is
/// used as the base for relative paths; absolute paths are used
/// verbatim.
///
/// A "path-like token" is a whitespace-separated chunk that starts
/// with `/`, `./`, `../`, or `~/`, OR contains a `/` and matches a
/// real filesystem entry when joined with cwd. Tokens are stripped
/// of common trailing punctuation (`,`, `.`, `:`, `;`, `!`, `?`,
/// backticks, parens, brackets, braces, quotes) before resolution —
/// operator prose tends to punctuate paths. URL-scheme tokens
/// (`http://…`) are skipped at the textual layer.
pub fn grant_paths_from_text(store: &ConsentStore, cwd: &Path, text: &str) -> Vec<GrantedPath> {
    let mut added: Vec<GrantedPath> = Vec::new();
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
            let suspicious = is_suspicious_grant(&dir);
            if !added.iter().any(|g| g.dir == dir) {
                added.push(GrantedPath { dir, suspicious });
            }
        }
    }
    added
}

/// Heuristic: is this grant unusually wide? `/usr`, `/etc`, `/var`,
/// `/opt`, `/lib*`, or the operator's own `$HOME` would be flagged.
/// A grant under those (e.g. `/usr/local/myproj`) is fine — only
/// the bare top-level dir trips this.
fn is_suspicious_grant(dir: &Path) -> bool {
    // Bare-tree exact matches.
    let suspicious_exact: &[&str] = &[
        "/usr", "/etc", "/var", "/opt", "/lib", "/lib64", "/bin", "/sbin", "/boot", "/srv", "/sys",
        "/proc", "/root", "/home",
    ];
    if let Some(s) = dir.to_str() {
        if suspicious_exact.contains(&s) {
            return true;
        }
    }
    // The operator's exact $HOME is also wide — covers the entire
    // user account.
    if let Some(home) = dirs::home_dir() {
        if dir == home {
            return true;
        }
    }
    false
}

fn strip_token_punctuation(s: &str) -> &str {
    // Trim asymmetrically. Operator prose wraps paths with leading
    // quote-like chars (`` `/etc/hosts` ``, `"./foo"`, `(../bar)`)
    // and trails them with sentence punctuation (`foo.c,` `foo.c.`
    // `see foo:`). Stripping `.` from the LEFT would eat the leading
    // dots of `./foo` and `../foo`, turning a relative path into an
    // absolute one that doesn't exist — so left-trim is limited to
    // chars that can never legitimately start a path.
    let left_trimmed = s.trim_start_matches(['`', '(', '[', '{', '\'', '"', '<']);
    left_trimmed.trim_end_matches([
        ',', '.', ':', ';', '!', '?', '`', ')', ']', '}', '\'', '"', '>',
    ])
}

fn looks_like_path(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Bare separators that operator prose uses as conjunctions —
    // `foo / bar`, `yes / no` — split into a lone "/" token. Never
    // a real path mention; resolves to filesystem root if we let it
    // through, which would consent-grant the entire FS. Same logic
    // for the lone parent / current-dir tokens which can sneak in
    // when prompts use `..` or `.` as ellipsis.
    if matches!(s, "/" | "." | "..") {
        return false;
    }
    // Skip URL-scheme tokens — `https://github.com/x`, `s3://bucket/key`,
    // `git+ssh://host/repo`. They contain `/` and would otherwise
    // burn a stat() syscall in resolve_candidate. Scheme syntax
    // (RFC 3986): first char ascii-alpha, rest ascii-alphanumeric
    // or `+`, `-`, `.`.
    if let Some(i) = s.find("://") {
        let scheme = &s[..i];
        let mut chars = scheme.chars();
        let first_ok = chars
            .next()
            .map(|c| c.is_ascii_alphabetic())
            .unwrap_or(false);
        let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if first_ok && rest_ok {
            return false;
        }
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
        assert!(added.iter().any(|g| g.dir == canon), "added={added:?}");
        // tmpdir-based grant should not trip the suspicion flag.
        assert!(added.iter().all(|g| !g.suspicious));
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
        assert!(added.iter().any(|g| g.dir == canon), "added={added:?}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn text_scanner_preserves_leading_dot_dot() {
        // Regression: a prompt like "read ../sibling/report.md," used
        // to get both leading dots stripped by the symmetric
        // trim_matches, turning it into "/sibling/report.md" which
        // doesn't exist — so consent was silently never granted.
        let tmp = std::env::temp_dir();
        let base = tmp.join(format!("kres-scan-dotdot-{}", std::process::id()));
        let sibling = base.join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        let file = sibling.join("report.md");
        std::fs::write(&file, b"x").unwrap();
        let cwd = base.join("here");
        std::fs::create_dir_all(&cwd).unwrap();
        let s = ConsentStore::new();
        let msg = "read ../sibling/report.md, write a fix";
        let added = grant_paths_from_text(&s, &cwd, msg);
        let canon_parent = sibling.canonicalize().unwrap();
        assert!(
            added.iter().any(|g| g.dir == canon_parent),
            "added={added:?}"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn strip_token_punctuation_keeps_leading_relative_prefixes() {
        assert_eq!(strip_token_punctuation("./foo,"), "./foo");
        assert_eq!(strip_token_punctuation("../foo."), "../foo");
        assert_eq!(strip_token_punctuation("`../foo`"), "../foo");
        assert_eq!(strip_token_punctuation("(./bar)"), "./bar");
        assert_eq!(strip_token_punctuation("\"../baz\""), "../baz");
    }

    #[test]
    fn text_scanner_ignores_non_paths() {
        let s = ConsentStore::new();
        let added = grant_paths_from_text(&s, Path::new("/tmp"), "just some text no paths here");
        assert!(added.is_empty());
    }

    #[test]
    fn text_scanner_ignores_bare_slash_separator() {
        // Regression: a prompt with prose like "lists of `foo` /
        // `bar`" used to produce a "/" token from split_whitespace,
        // which resolved to filesystem root and granted `/` — making
        // is_allowed() true for every absolute path on the host.
        let s = ConsentStore::new();
        let added = grant_paths_from_text(
            &s,
            Path::new("/tmp"),
            "lists of `relevant_symbols` / `relevant_file_sections` and yes / no / n/a",
        );
        assert!(added.is_empty(), "bare separators must not grant: {added:?}");
        assert!(!s.is_allowed(Path::new("/etc/passwd")));
    }

    #[test]
    fn grant_from_mention_refuses_filesystem_root() {
        let s = ConsentStore::new();
        let got = s.grant_from_mention(Path::new("/"));
        assert!(got.is_none(), "granting / would defeat the consent gate");
        assert!(!s.is_allowed(Path::new("/etc/passwd")));
    }

    #[test]
    fn is_suspicious_grant_flags_top_level_system_dirs() {
        for d in ["/usr", "/etc", "/var", "/opt", "/bin", "/lib", "/home"] {
            assert!(is_suspicious_grant(Path::new(d)), "{d} should be flagged");
        }
    }

    #[test]
    fn is_suspicious_grant_does_not_flag_sub_paths() {
        for d in ["/usr/local/myproj", "/etc/myapp", "/var/log/specific"] {
            assert!(
                !is_suspicious_grant(Path::new(d)),
                "{d} should NOT be flagged"
            );
        }
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

    #[test]
    fn looks_like_path_skips_url_schemes() {
        assert!(!looks_like_path("http://example.com/foo"));
        assert!(!looks_like_path(
            "https://github.com/masoncl/review-prompts"
        ));
        assert!(!looks_like_path("s3://bucket/key"));
        assert!(!looks_like_path("ftp://host/p"));
        // But a path with a colon in a non-scheme position still
        // matches (e.g. "fs/btrfs/ctree.c:123" is a citation form
        // we want to grant the file's parent for).
        assert!(looks_like_path("fs/btrfs/ctree.c:123"));
    }
}
