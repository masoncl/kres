//! Per-session turn logger.
//!
//! Mirrors Every agent
//! round-trip (user turn + assistant turn) is appended to JSONL files
//! under `<base_dir>/.kres/logs/<session-uuid>/`:
//!
//! - `code.jsonl` — fast + slow + consolidator + lens-merge inferences
//! - `main.jsonl` — main agent + todo agent + goal define/check +
//!   findings merge + summary inferences
//!
//! The session UUID is derived deterministically from pid + now()
//! (uuid5 over NAMESPACE_OID) so rerunning the same process twice at
//! the same instant does not collide (pid disambiguates).
//!
//! Writes are serialised behind a single Mutex, so logging from
//! multiple tokio tasks is safe without further coordination.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use uuid::Uuid;

/// UUID namespace used for session-id derivation. This is the
/// well-known NAMESPACE_OID value (`6ba7b812-9dad-11d1-80b4-00c04fd430c8`)
/// and
const NAMESPACE_OID: Uuid = Uuid::from_bytes([
    0x6b, 0xa7, 0xb8, 0x12, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
]);

/// One row in a log file. `usage` carries the server-reported token
/// breakdown; `thinking` captures the slow agent's reasoning stream
/// when it is available; `request` snapshots wire-relevant request
/// config (model, max_tokens, thinking shape) for user-side records
/// so log readers can confirm what was actually asked of the model.
#[derive(Debug, Serialize)]
struct LogEntry<'a> {
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<&'a str>,
    content: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<LoggedUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request: Option<&'a RequestMeta>,
}

/// Wire-relevant request config snapshot. Populated on user-side
/// log records for calls that go to a Claude / OpenAI completion
/// endpoint. Fields that don't apply (e.g. `effort` for a
/// non-adaptive thinking shape, `budget_tokens` for adaptive) are
/// omitted via `skip_serializing_if`.
#[derive(Debug, Serialize, Clone, Default)]
pub struct RequestMeta {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

#[derive(Debug, Serialize, Clone, Copy, Default)]
pub struct LoggedUsage {
    pub input: u64,
    pub output: u64,
    #[serde(skip_serializing_if = "is_zero", default)]
    pub cache_creation: u64,
    #[serde(skip_serializing_if = "is_zero", default)]
    pub cache_read: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Append-only session logger. Drop closes the file handles.
pub struct TurnLogger {
    session_id: String,
    session_dir: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    code: File,
    main: File,
}

impl TurnLogger {
    /// Create a new logger rooted at `<base_dir>/.kres/logs/<uuid>/`.
    /// The `.kres/logs/<uuid>` layout mirrors exactly so existing
    /// log-inspection tools port over as-is.
    pub fn new(base_dir: &Path) -> io::Result<Self> {
        let now = chrono::Local::now();
        let seed = format!("{}-{}", std::process::id(), now.to_rfc3339());
        let uuid = Uuid::new_v5(&NAMESPACE_OID, seed.as_bytes());
        let session_id = uuid.to_string();
        let session_dir = base_dir.join(".kres").join("logs").join(&session_id);
        std::fs::create_dir_all(&session_dir)?;
        let code = OpenOptions::new()
            .create(true)
            .append(true)
            .open(session_dir.join("code.jsonl"))?;
        let main = OpenOptions::new()
            .create(true)
            .append(true)
            .open(session_dir.join("main.jsonl"))?;
        Ok(Self {
            session_id,
            session_dir,
            inner: Mutex::new(Inner { code, main }),
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    /// Append to `code.jsonl`. Swallows I/O errors after logging a
    /// warning — the REPL should keep running even if the disk is
    /// full, matching write semantics.
    pub fn log_code(
        &self,
        role: &str,
        content: &str,
        usage: Option<LoggedUsage>,
        thinking: Option<&str>,
    ) {
        self.log_code_labeled(role, None, content, usage, thinking);
    }

    /// Append to `code.jsonl` with a stable machine label such as
    /// `step=review lens=memory phase=slow`. The label is metadata,
    /// not prompt content, so it does not perturb cache prefixes or
    /// agent behavior.
    pub fn log_code_labeled(
        &self,
        role: &str,
        label: Option<&str>,
        content: &str,
        usage: Option<LoggedUsage>,
        thinking: Option<&str>,
    ) {
        self.log_code_labeled_with_request(role, label, content, usage, thinking, None);
    }

    /// Like `log_code_labeled` but also serialises a `request` block
    /// describing the wire-format request config (model, max_tokens,
    /// thinking shape). Use this from call sites that go straight to
    /// a completion endpoint so reviewers can verify what was asked.
    pub fn log_code_labeled_with_request(
        &self,
        role: &str,
        label: Option<&str>,
        content: &str,
        usage: Option<LoggedUsage>,
        thinking: Option<&str>,
        request: Option<&RequestMeta>,
    ) {
        if let Err(e) = self.write(true, role, label, content, usage, thinking, request) {
            tracing::warn!(target: "kres_core::log", "code log write failed: {e}");
        }
    }

    /// Append to `main.jsonl`. Same semantics as `log_code`.
    pub fn log_main(
        &self,
        role: &str,
        content: &str,
        usage: Option<LoggedUsage>,
        thinking: Option<&str>,
    ) {
        if let Err(e) = self.write(false, role, None, content, usage, thinking, None) {
            tracing::warn!(target: "kres_core::log", "main log write failed: {e}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        is_code: bool,
        role: &str,
        label: Option<&str>,
        content: &str,
        usage: Option<LoggedUsage>,
        thinking: Option<&str>,
        request: Option<&RequestMeta>,
    ) -> io::Result<()> {
        let entry = LogEntry {
            role,
            label,
            content,
            usage,
            thinking,
            request,
        };
        let line = serde_json::to_string(&entry).map_err(io::Error::other)?;
        let mut guard = self.inner.lock().unwrap();
        let f = if is_code {
            &mut guard.code
        } else {
            &mut guard.main
        };
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        f.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::tempdir;

    #[test]
    fn creates_session_dir_and_writes_entries() {
        let dir = tempdir().unwrap();
        let log = TurnLogger::new(dir.path()).unwrap();
        log.log_code(
            "user",
            "hello",
            Some(LoggedUsage {
                input: 10,
                output: 0,
                ..Default::default()
            }),
            None,
        );
        log.log_code(
            "assistant",
            "hi",
            Some(LoggedUsage {
                input: 0,
                output: 5,
                ..Default::default()
            }),
            Some("thought"),
        );
        log.log_main("user", "plan", None, None);
        drop(log);

        // session dir is .kres/logs/<uuid>
        let logs = dir.path().join(".kres").join("logs");
        let session_dirs: Vec<_> = std::fs::read_dir(&logs)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(session_dirs.len(), 1);
        let session = &session_dirs[0].path();
        let mut code = String::new();
        File::open(session.join("code.jsonl"))
            .unwrap()
            .read_to_string(&mut code)
            .unwrap();
        assert_eq!(code.lines().count(), 2);
        assert!(code.contains("\"role\":\"user\""));
        assert!(code.contains("\"thinking\":\"thought\""));

        let mut main = String::new();
        File::open(session.join("main.jsonl"))
            .unwrap()
            .read_to_string(&mut main)
            .unwrap();
        assert_eq!(main.lines().count(), 1);
        assert!(main.contains("\"role\":\"user\""));
        assert!(!main.contains("\"usage\""));
    }

    #[test]
    fn session_ids_differ_between_instances() {
        let dir = tempdir().unwrap();
        let a = TurnLogger::new(dir.path()).unwrap();
        // tiny sleep to ensure the timestamp differs at rfc3339 sub-second
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = TurnLogger::new(dir.path()).unwrap();
        assert_ne!(a.session_id(), b.session_id());
    }

    #[test]
    fn cache_tokens_omit_when_zero() {
        let dir = tempdir().unwrap();
        let log = TurnLogger::new(dir.path()).unwrap();
        log.log_code(
            "assistant",
            "",
            Some(LoggedUsage {
                input: 1,
                output: 1,
                cache_creation: 0,
                cache_read: 0,
            }),
            None,
        );
        drop(log);
        let logs = dir.path().join(".kres").join("logs");
        let session = std::fs::read_dir(&logs)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let s = std::fs::read_to_string(session.join("code.jsonl")).unwrap();
        assert!(!s.contains("cache_creation"));
        assert!(!s.contains("cache_read"));
    }

    #[test]
    fn code_log_emits_request_meta_when_provided() {
        let dir = tempdir().unwrap();
        let log = TurnLogger::new(dir.path()).unwrap();
        let meta = RequestMeta {
            model: "claude-x".into(),
            max_tokens: 128_000,
            thinking: Some("adaptive".into()),
            effort: Some("xhigh".into()),
            budget_tokens: None,
        };
        log.log_code_labeled_with_request(
            "user",
            Some("phase=slow task=research"),
            "{}",
            None,
            None,
            Some(&meta),
        );
        // No-meta path should omit the field entirely.
        log.log_code_labeled("user", Some("phase=fast-gather"), "{}", None, None);
        drop(log);

        let logs = dir.path().join(".kres").join("logs");
        let session = std::fs::read_dir(&logs)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let s = std::fs::read_to_string(session.join("code.jsonl")).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2);
        // First line carries the request block with xhigh.
        assert!(lines[0].contains("\"request\":"));
        assert!(lines[0].contains("\"effort\":\"xhigh\""));
        assert!(lines[0].contains("\"thinking\":\"adaptive\""));
        assert!(lines[0].contains("\"model\":\"claude-x\""));
        assert!(!lines[0].contains("\"budget_tokens\""));
        // Second line (no meta) must NOT carry a request block.
        assert!(!lines[1].contains("\"request\":"));
    }

    #[test]
    fn code_log_preserves_label_metadata() {
        let dir = tempdir().unwrap();
        let log = TurnLogger::new(dir.path()).unwrap();
        log.log_code_labeled(
            "assistant",
            Some("phase=slow step=review lens=memory"),
            "{}",
            None,
            None,
        );
        drop(log);

        let logs = dir.path().join(".kres").join("logs");
        let session = std::fs::read_dir(&logs)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let s = std::fs::read_to_string(session.join("code.jsonl")).unwrap();
        assert!(s.contains("\"label\":\"phase=slow step=review lens=memory\""));
        assert!(s.contains("\"content\":\"{}\""));
    }
}
