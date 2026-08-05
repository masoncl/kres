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

use std::collections::{BTreeMap, HashSet};
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
    /// UTC wall-clock time at which this record was appended. User and
    /// assistant records for the same label delimit one model call.
    timestamp: String,
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<&'a str>,
    content: &'a str,
    /// Exact model-visible conversation for calls whose request contains
    /// earlier turns in addition to `content`. `content` remains the newest
    /// turn for compatibility with existing log readers.
    #[serde(skip_serializing_if = "Option::is_none")]
    request_content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<LoggedUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request: Option<&'a RequestMeta>,
    /// Model identifier reported by the provider for an assistant response.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_model: Option<&'a str>,
    /// Deterministic accounting over the exact logged user payload. This is
    /// diagnostic metadata only and is never sent to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    context_stats: Option<&'a ContextStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextStats {
    pub serialized_chars: usize,
    pub system_chars: usize,
    pub content_fingerprint: String,
    pub field_chars: BTreeMap<String, usize>,
    pub category_chars: BTreeMap<String, usize>,
    pub whole_file_scan_occurrences: usize,
    pub duplicate_context_items: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

fn stable_payload_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn context_category(field: &str) -> &'static str {
    match field {
        "skills" | "common_skills" => "skills",
        "symbols" | "context" => "source_evidence",
        "previous_findings" | "lens_outputs" | "existing_findings" | "analysis"
        | "analysis_summary" | "new_followups" | "rejected_response" => "reused_model_output",
        "instructions" | "parallel_lenses" | "lens_instruction" | "schema" | "contract"
        | "validation_errors" => "instructions_schema",
        "plan" | "question" | "original_prompt" | "current_todo" | "goal" | "lenses"
        | "completed_query" | "query" | "task" | "task_brief" | "mode" => "workflow_state",
        _ => "other",
    }
}

/// Read one turn's text as JSON documents.
///
/// A prompt is sent as a stable document followed by a per-call delta
/// document, concatenated. Both halves must be accounted for, so parse the
/// text as a stream of whitespace-separated JSON values rather than a single
/// one. A payload that is one document still yields exactly one entry.
fn turn_documents(text: &str) -> Vec<serde_json::Value> {
    serde_json::Deserializer::from_str(text)
        .into_iter::<serde_json::Value>()
        .map_while(Result::ok)
        .collect()
}

/// Every top-level JSON object in a payload, in order: across conversation
/// turns when the payload is a `messages` envelope, and across the stable and
/// delta documents within each turn.
fn payload_turns(content: &str) -> Vec<serde_json::Value> {
    let documents = turn_documents(content);
    let is_envelope = documents.len() == 1
        && documents[0]
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .is_some();
    if !is_envelope {
        return documents;
    }
    documents[0]["messages"]
        .as_array()
        .expect("checked above")
        .iter()
        .filter_map(|message| message.get("content").and_then(serde_json::Value::as_str))
        .flat_map(turn_documents)
        .collect()
}

/// Count normalized symbol bodies that also appear verbatim inside a raw
/// context entry — the exact duplication Phase 3 removed.
///
/// This is O(symbols x context) substring search over the whole payload, so it
/// is NOT run when logging. Structural tests assert the invariant instead; a
/// per-write quadratic scan to re-detect something the fetchers now make
/// impossible by construction is not worth the wall-clock on multi-megabyte
/// lens prompts.
pub fn duplicate_symbol_bodies_in_context(content: &str) -> usize {
    let turns = payload_turns(content);
    let definitions: Vec<&str> = turns
        .iter()
        .filter_map(|turn| turn.get("symbols"))
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .filter_map(|symbol| symbol.get("definition").and_then(serde_json::Value::as_str))
        .filter(|definition| !definition.is_empty())
        .collect();
    let bodies: Vec<&str> = turns
        .iter()
        .filter_map(|turn| turn.get("context"))
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .filter_map(|item| item.get("content").and_then(serde_json::Value::as_str))
        .collect();
    definitions
        .iter()
        .filter(|definition| bodies.iter().any(|body| body.contains(**definition)))
        .count()
}

impl ContextStats {
    /// Classify one fully assembled user payload without modifying it. This is
    /// also the read-only accounting entry point for offline log tooling.
    ///
    /// Every step here is linear in the payload: one parse, one size pass per
    /// top-level field, one hashed pass over context entries, and one marker
    /// count. Anything super-linear belongs in a test, not on the write path.
    pub fn from_user_content(content: &str) -> Self {
        Self::from_user_content_and_request(content, None)
    }

    pub fn from_user_content_and_request(content: &str, request: Option<&RequestMeta>) -> Self {
        let turns = payload_turns(content);
        // A `messages` envelope is a logging wrapper, not something the model
        // sees; account for the turn text inside it rather than its framing.
        let conversation_chars = turn_documents(content)
            .first()
            .and_then(|value| value.get("messages").cloned())
            .and_then(|messages| {
                Some(
                    messages
                        .as_array()?
                        .iter()
                        .filter_map(|m| m.get("content").and_then(serde_json::Value::as_str))
                        .map(str::len)
                        .sum::<usize>(),
                )
            });

        let mut field_chars: BTreeMap<String, usize> = BTreeMap::new();
        let mut category_chars: BTreeMap<String, usize> = BTreeMap::new();
        let mut duplicate_context_items = 0;
        let mut seen_context: HashSet<String> = HashSet::new();

        for turn in &turns {
            let Some(fields) = turn.as_object() else {
                continue;
            };
            for (name, value) in fields {
                let chars = serde_json::to_string(value).map_or(0, |encoded| encoded.len());
                *field_chars.entry(name.clone()).or_insert(0) += chars;
                *category_chars
                    .entry(context_category(name).to_string())
                    .or_insert(0) += chars;
            }
            for item in fields
                .get("context")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
            {
                let encoded = serde_json::to_string(item).unwrap_or_default();
                if !seen_context.insert(encoded) {
                    duplicate_context_items += 1;
                }
            }
        }

        if turns.is_empty() {
            category_chars.insert("other".into(), content.len());
        }

        // A delimited scan has both a start marker and an
        // `END WHOLE-FILE RISK SCAN` marker. Count only starts so one
        // well-formed block does not diagnose itself as a duplicate.
        let whole_file_scan_occurrences = content
            .matches("WHOLE-FILE RISK SCAN")
            .count()
            .saturating_sub(content.matches("END WHOLE-FILE RISK SCAN").count());

        let serialized_chars = conversation_chars.unwrap_or(content.len());
        let system_chars = request.map_or(0, |meta| meta.system_chars);
        if system_chars > 0 {
            category_chars.insert("system_prompt".into(), system_chars);
        }

        let mut warnings = Vec::new();
        if whole_file_scan_occurrences > 1 {
            warnings.push("duplicate whole-file risk scan".into());
        }
        if duplicate_context_items > 0 {
            warnings.push("exact duplicate context items".into());
        }
        let skill_chars = field_chars.get("skills").copied().unwrap_or(0)
            + field_chars.get("common_skills").copied().unwrap_or(0);
        if skill_chars > 80_000 {
            warnings.push("skills payload exceeds 80000 characters".into());
        }
        if serialized_chars > 1_000_000 {
            warnings.push("request payload exceeds 1000000 characters".into());
        }

        Self {
            serialized_chars,
            system_chars,
            content_fingerprint: format!("{:016x}", stable_payload_hash(content.as_bytes())),
            field_chars,
            category_chars,
            whole_file_scan_occurrences,
            duplicate_context_items,
            warnings,
        }
    }
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
    #[serde(skip_serializing_if = "is_zero_usize", default)]
    pub system_chars: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
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

fn is_zero_usize(n: &usize) -> bool {
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

    /// Log an assistant response with the model identifier returned by the provider.
    pub fn log_code_labeled_with_model(
        &self,
        role: &str,
        label: Option<&str>,
        content: &str,
        usage: Option<LoggedUsage>,
        thinking: Option<&str>,
        response_model: Option<&str>,
    ) {
        let context_stats = (role == "user").then(|| ContextStats::from_user_content(content));
        let entry = LogEntry {
            timestamp: log_timestamp(),
            role,
            label,
            content,
            request_content: None,
            usage,
            thinking,
            request: None,
            response_model,
            context_stats: context_stats.as_ref(),
        };
        if let Err(e) = self.write(true, &entry) {
            tracing::warn!(target: "kres_core::log", "code log write failed: {e}");
        }
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
        let context_stats =
            (role == "user").then(|| ContextStats::from_user_content_and_request(content, request));
        let entry = LogEntry {
            timestamp: log_timestamp(),
            role,
            label,
            content,
            request_content: None,
            usage,
            thinking,
            request,
            response_model: None,
            context_stats: context_stats.as_ref(),
        };
        if let Err(e) = self.write(true, &entry) {
            tracing::warn!(target: "kres_core::log", "code log write failed: {e}");
        }
    }

    /// Log a multi-turn call while retaining both the newest turn in `content`
    /// and the complete model-visible conversation in `request_content`.
    pub fn log_code_user_request_content(
        &self,
        label: Option<&str>,
        content: &str,
        request_content: &str,
        request: Option<&RequestMeta>,
    ) {
        let context_stats = ContextStats::from_user_content_and_request(request_content, request);
        let entry = LogEntry {
            timestamp: log_timestamp(),
            role: "user",
            label,
            content,
            request_content: Some(request_content),
            usage: None,
            thinking: None,
            request,
            response_model: None,
            context_stats: Some(&context_stats),
        };
        if let Err(e) = self.write(true, &entry) {
            tracing::warn!(target: "kres_core::log", "code log write failed: {e}");
        }
    }

    /// Append to `main.jsonl`. Same semantics as `log_code`.
    pub fn log_main(
        &self,
        role: &str,
        label: Option<&str>,
        content: &str,
        usage: Option<LoggedUsage>,
        thinking: Option<&str>,
    ) {
        let context_stats = (role == "user").then(|| ContextStats::from_user_content(content));
        let entry = LogEntry {
            timestamp: log_timestamp(),
            role,
            label,
            content,
            request_content: None,
            usage,
            thinking,
            request: None,
            response_model: None,
            context_stats: context_stats.as_ref(),
        };
        if let Err(e) = self.write(false, &entry) {
            tracing::warn!(target: "kres_core::log", "main log write failed: {e}");
        }
    }

    /// Append to `main.jsonl` with the exact request configuration used by a
    /// direct main/todo/goal inference call.
    pub fn log_main_with_request(
        &self,
        role: &str,
        label: Option<&str>,
        content: &str,
        usage: Option<LoggedUsage>,
        thinking: Option<&str>,
        request: Option<&RequestMeta>,
    ) {
        let context_stats =
            (role == "user").then(|| ContextStats::from_user_content_and_request(content, request));
        let entry = LogEntry {
            timestamp: log_timestamp(),
            role,
            label,
            content,
            request_content: None,
            usage,
            thinking,
            request,
            response_model: None,
            context_stats: context_stats.as_ref(),
        };
        if let Err(e) = self.write(false, &entry) {
            tracing::warn!(target: "kres_core::log", "main log write failed: {e}");
        }
    }

    fn write(&self, is_code: bool, entry: &LogEntry<'_>) -> io::Result<()> {
        let line = serde_json::to_string(entry).map_err(io::Error::other)?;
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

fn log_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
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
        log.log_main("user", Some("phase=todo"), "plan", None, None);
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
        for line in code.lines() {
            let entry: serde_json::Value = serde_json::from_str(line).unwrap();
            let timestamp = entry["timestamp"].as_str().unwrap();
            chrono::DateTime::parse_from_rfc3339(timestamp).unwrap();
        }
        assert!(code.contains("\"role\":\"user\""));
        assert!(code.contains("\"thinking\":\"thought\""));

        let mut main = String::new();
        File::open(session.join("main.jsonl"))
            .unwrap()
            .read_to_string(&mut main)
            .unwrap();
        assert_eq!(main.lines().count(), 1);
        assert!(main.contains("\"timestamp\":"));
        assert!(main.contains("\"role\":\"user\""));
        assert!(!main.contains("\"usage\""));
    }

    #[test]
    fn assistant_response_model_is_logged_without_changing_label() {
        let dir = tempdir().unwrap();
        let log = TurnLogger::new(dir.path()).unwrap();
        log.log_code_labeled_with_model(
            "assistant",
            Some("phase=slow"),
            "done",
            None,
            None,
            Some("claude-opus-4-8"),
        );
        let path = log.session_dir().join("code.jsonl");
        drop(log);

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(value["label"], "phase=slow");
        assert_eq!(value["response_model"], "claude-opus-4-8");
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
            system_chars: 42,
            system_fingerprint: Some("abcd".into()),
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
        assert!(s.contains("\"system_chars\":42"));
        assert!(s.contains("\"system_prompt\":42"));
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
    fn multi_turn_log_accounts_for_and_preserves_the_complete_request() {
        let dir = tempdir().unwrap();
        let log = TurnLogger::new(dir.path()).unwrap();
        let first = r#"{"question":"original","symbols":[{"definition":"body"}],"context":[{"content":"body"}]}"#;
        let newest = r#"{"question":"continue","context":[{"content":"body"}]}"#;
        let complete = serde_json::to_string(&serde_json::json!({
            "messages": [
                {"role": "user", "content": first},
                {"role": "assistant", "content": "gather more"},
                {"role": "user", "content": newest},
            ]
        }))
        .unwrap();
        log.log_code_user_request_content(
            Some("phase=fast-gather round=2"),
            newest,
            &complete,
            None,
        );
        let path = log.session_dir().join("code.jsonl");
        drop(log);

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(value["content"], newest);
        assert_eq!(value["request_content"], complete);
        let visible_chars = first.len() + "gather more".len() + newest.len();
        assert_eq!(value["context_stats"]["serialized_chars"], visible_chars);
        assert!(value["context_stats"]["field_chars"]["question"]
            .as_u64()
            .is_some_and(|chars| chars > 0));
        assert!(value["context_stats"]["field_chars"]["symbols"]
            .as_u64()
            .is_some_and(|chars| chars > 0));
        assert!(value["context_stats"]["field_chars"]["context"]
            .as_u64()
            .is_some_and(|chars| chars > 0));
        assert_eq!(value["context_stats"]["duplicate_context_items"], 1);
        // Cross-turn body retransmission stays detectable, just off the write
        // path: the helper walks every turn of the complete request.
        assert_eq!(duplicate_symbol_bodies_in_context(&complete), 1);
    }

    #[test]
    fn main_log_records_carry_a_phase_label() {
        // main.jsonl had no label parameter at all, so goal, todo, main-agent
        // and compact calls could not be told apart or paired with their
        // responses. Wall-time analysis had to guess by file position.
        let dir = tempdir().unwrap();
        let log = TurnLogger::new(dir.path()).unwrap();
        log.log_main_with_request("user", Some("phase=goal check"), "{}", None, None, None);
        log.log_main(
            "assistant",
            Some("phase=goal check"),
            "met",
            Some(LoggedUsage {
                input: 5,
                output: 1,
                cache_creation: 0,
                cache_read: 0,
            }),
            None,
        );
        let path = log.session_dir().join("main.jsonl");
        drop(log);

        let rows: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(r["label"], "phase=goal check");
        }
    }

    #[test]
    fn an_assistant_record_can_be_paired_with_its_request_by_label() {
        // Usage lives on the assistant record and the phase lives on the
        // label. An unlabelled assistant record is unattributable: consolidate
        // and promote logged their requests as `phase=consolidate` /
        // `phase=promote` but their responses with no label at all, so a
        // by-stage accounting folded 825.6k of fresh input into whichever
        // bucket it used for unlabelled records.
        let dir = tempdir().unwrap();
        let log = TurnLogger::new(dir.path()).unwrap();
        log.log_code_labeled("user", Some("phase=consolidate"), "{}", None, None);
        log.log_code_labeled(
            "assistant",
            Some("phase=consolidate"),
            "done",
            Some(LoggedUsage {
                input: 10,
                output: 2,
                cache_creation: 0,
                cache_read: 0,
            }),
            None,
        );
        let path = log.session_dir().join("code.jsonl");
        drop(log);

        let rows: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let with_usage: Vec<_> = rows.iter().filter(|r| r.get("usage").is_some()).collect();
        assert_eq!(with_usage.len(), 1);
        assert_eq!(
            with_usage[0]["label"], "phase=consolidate",
            "every record carrying usage must name its phase"
        );
    }

    #[test]
    fn accounting_covers_both_documents_of_a_split_prompt() {
        // A prompt is sent as a stable document plus a per-call delta,
        // concatenated. If accounting parsed only the first value, every
        // field in the delta would vanish from the category totals and the
        // whole payload would be misfiled as "other".
        let stable = "{\n  \"question\": \"q\",\n  \"skills\": {\"kernel\": \"body\"}\n}\n";
        let delta = "{\n  \"symbols\": [{\"definition\": \"int f(void) {}\"}]\n}";
        let rendered = format!("{stable}{delta}");

        let stats = ContextStats::from_user_content(&rendered);

        assert!(stats.field_chars.contains_key("question"));
        assert!(stats.field_chars.contains_key("skills"));
        assert!(
            stats.field_chars.contains_key("symbols"),
            "delta fields must be accounted for: {:?}",
            stats.field_chars
        );
        assert!(stats.category_chars.contains_key("source_evidence"));
        assert!(stats.category_chars.contains_key("skills"));
        assert!(
            !stats.category_chars.contains_key("other"),
            "a split prompt must not fall through to the unparsed bucket"
        );
        assert_eq!(stats.serialized_chars, rendered.len());
    }

    #[test]
    fn duplicate_detection_sees_across_the_document_boundary() {
        // The stable half holds the symbol, the delta half holds a raw context
        // copy of the same body. Splitting the prompt must not hide that.
        let rendered = concat!(
            "{\n  \"symbols\": [{\"definition\": \"int f(void) {}\"}]\n}\n",
            "{\n  \"context\": [{\"content\": \"Body:\\nint f(void) {}\"}]\n}"
        );

        assert_eq!(duplicate_symbol_bodies_in_context(rendered), 1);
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

    #[test]
    fn user_log_records_structured_context_accounting_and_duplicates() {
        let dir = tempdir().unwrap();
        let log = TurnLogger::new(dir.path()).unwrap();
        let prompt = serde_json::json!({
            "question": "WHOLE-FILE RISK SCAN\nWHOLE-FILE RISK SCAN",
            "skills": {"kernel": {"content": "guide"}},
            "symbols": [{"name":"foo","definition":"int foo(void) {}"}],
            "context": [{"source":"mcp:source:foo","content":"Body:\nint foo(void) {}"}]
        });
        log.log_code("user", &prompt.to_string(), None, None);
        let path = log.session_dir().join("code.jsonl");
        drop(log);

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let stats = &value["context_stats"];
        assert_eq!(stats["whole_file_scan_occurrences"], 2);
        // The quadratic symbol-body-in-context scan is deliberately not on the
        // write path; it is available to tests as a standalone helper.
        assert!(stats.get("duplicate_symbol_context_bodies").is_none());
        assert_eq!(duplicate_symbol_bodies_in_context(&prompt.to_string()), 1);
        assert!(stats["field_chars"]["skills"].as_u64().unwrap() > 0);
        assert!(stats["category_chars"]["source_evidence"].as_u64().unwrap() > 0);
        assert!(stats["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning == "duplicate whole-file risk scan"));
    }

    #[test]
    fn delimited_scan_counts_as_one_block() {
        let content = serde_json::json!({
            "question": "--- WHOLE-FILE RISK SCAN ---\n{}\n--- END WHOLE-FILE RISK SCAN ---"
        })
        .to_string();
        let stats = ContextStats::from_user_content(&content);

        assert_eq!(stats.whole_file_scan_occurrences, 1);
        assert!(!stats
            .warnings
            .iter()
            .any(|warning| warning == "duplicate whole-file risk scan"));
    }
}
