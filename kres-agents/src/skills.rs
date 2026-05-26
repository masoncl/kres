//! Skill loading.
//!
//! A "skill" is a markdown file with YAML-ish frontmatter. Its body is
//! passed to the code agents as domain-specific guidance. Files
//! referenced from the body (via absolute-path backticks) are
//! pre-loaded into the skill's `files` map so the agent doesn't have
//! to request them via `skill_reads` every time.
//!
//! Invariants owed (bugs.md#M6): had a single
//! global skill that accumulated every loaded file across tasks. Here
//! each Skill owns its own `files` map and there is no cross-skill
//! mutation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::AgentError;
use crate::workspace::WorkspaceProfile;

#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub invocation_policy: InvocationPolicy,
    pub content: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum InvocationPolicy {
    /// Always loaded at session start. Matches `invocation_policy:
    /// automatic` in the frontmatter.
    Automatic,
    /// Loaded when the user prompt mentions the skill name.
    #[default]
    Manual,
}

#[derive(Debug, Clone, Default)]
pub struct Skills {
    pub items: BTreeMap<String, Skill>,
}

impl Skills {
    /// Load every `*.md` under `dir` and pre-load referenced files.
    pub fn load_dir(dir: &Path) -> Result<Self, AgentError> {
        let mut items = BTreeMap::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(AgentError::from(err)),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let skill = Skill::from_path(&path)?;
            items.insert(skill.name.clone(), skill);
        }
        Ok(Self { items })
    }

    /// Return the skills that should be auto-loaded at session start
    /// for the detected workspace.
    pub fn auto_loaded(&self) -> Vec<&Skill> {
        self.items
            .values()
            .filter(|s| s.invocation_policy == InvocationPolicy::Automatic)
            .collect()
    }

    pub fn auto_loaded_for_workspace(&self, profile: &WorkspaceProfile) -> Vec<&Skill> {
        profile
            .knowledge_skills
            .iter()
            .filter_map(|name| self.items.get(*name))
            .filter(|s| s.invocation_policy == InvocationPolicy::Automatic)
            .collect()
    }

    /// Load only the automatic skills selected by workspace
    /// detection. Unlike `load_dir`, this never parses unrelated
    /// skill files, so one bad/manual skill cannot disable automatic
    /// workspace knowledge.
    pub fn load_auto_for_workspace(
        dir: &Path,
        profile: &WorkspaceProfile,
    ) -> Result<(Self, Vec<String>), AgentError> {
        let mut items = BTreeMap::new();
        let mut warnings = Vec::new();
        for name in &profile.knowledge_skills {
            let path = dir.join(format!("{name}.md"));
            match Skill::from_path(&path) {
                Ok(skill)
                    if skill.name == *name
                        && skill.invocation_policy == InvocationPolicy::Automatic =>
                {
                    items.insert(skill.name.clone(), skill);
                }
                Ok(skill) if skill.name != *name => warnings.push(format!(
                    "skill {} not auto-loaded: frontmatter name '{}' does not match requested skill '{}'",
                    path.display(),
                    skill.name,
                    name
                )),
                Ok(skill) => warnings.push(format!(
                    "skill {} not auto-loaded: invocation_policy is {:?}",
                    path.display(),
                    skill.invocation_policy
                )),
                Err(AgentError::Other(msg)) if msg.contains("No such file") => {
                    warnings.push(format!("skill {} not loaded: {msg}", path.display()));
                }
                Err(e) => return Err(e),
            }
        }
        Ok((Self { items }, warnings))
    }

    /// Produce the JSON value the code-agent prompt expects:
    /// `{"<skill_name>": {"content": "...", "files": {...}}, ...}`.
    pub fn to_prompt_value(&self, selected: &[&Skill]) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        for s in selected {
            m.insert(
                s.name.clone(),
                serde_json::json!({
                    "content": s.content,
                    "files": s.files,
                }),
            );
        }
        serde_json::Value::Object(m)
    }
}

impl Skill {
    pub fn from_path(path: &Path) -> Result<Self, AgentError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| AgentError::Other(format!("skill {}: {e}", path.display())))?;
        Skill::from_str_with_stem(
            &raw,
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string(),
        )
    }

    pub fn from_str_with_stem(raw: &str, default_name: String) -> Result<Self, AgentError> {
        let (fm, body) = split_frontmatter(raw);
        let meta = parse_frontmatter(fm);
        let name = meta
            .get("name")
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or(default_name);
        let description = meta.get("description").cloned().unwrap_or_default();
        let invocation_policy = match meta.get("invocation_policy").map(|s| s.as_str()) {
            Some("automatic") => InvocationPolicy::Automatic,
            Some("manual") | None => InvocationPolicy::Manual,
            Some(other) => {
                tracing::warn!(
                    target: "kres_agents",
                    skill = %name,
                    policy = %other,
                    "unknown invocation_policy, treating as manual"
                );
                InvocationPolicy::Manual
            }
        };
        let referenced = find_absolute_paths_in_backticks(body);
        let mut files = BTreeMap::new();
        for p in referenced {
            let pb = PathBuf::from(&p);
            if !pb.is_absolute() {
                continue;
            }
            match std::fs::read_to_string(&pb) {
                Ok(content) => {
                    files.insert(p, content);
                }
                Err(_) => {
                    // Path referenced but not on disk yet — don't
                    // fail; the code agent can request it via
                    // skill_reads.
                }
            }
        }
        Ok(Skill {
            name,
            description,
            invocation_policy,
            content: body.to_string(),
            files,
        })
    }
}

/// Split a markdown file into (frontmatter_text, body_text). Only
/// recognizes the simple `---\n...\n---\n` pattern at the very top.
fn split_frontmatter(raw: &str) -> (&str, &str) {
    if !raw.starts_with("---") {
        return ("", raw);
    }
    // Move past the opening `---\n`.
    let rest = &raw[3..];
    let rest = rest.trim_start_matches(['\r']).trim_start_matches('\n');
    let Some(end_rel) = rest.find("\n---") else {
        return ("", raw);
    };
    let fm = &rest[..end_rel];
    let after = &rest[end_rel + 4..];
    let after = after.trim_start_matches(['\r']).trim_start_matches('\n');
    (fm, after)
}

fn parse_frontmatter(fm: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for line in fm.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        m.insert(k.trim().to_string(), v.trim().to_string());
    }
    m
}

/// Extract every absolute path (starts with `/`) that appears inside
/// a single backtick span. We don't need to cover every markdown edge
/// case here — the convention is "absolute paths go inside
/// single backticks".
///
/// Paths inside triple-backtick fenced code blocks are deliberately
/// skipped: a skill that illustrates an example with `/proc/self/mem`
/// inside a ``` fence must not cause the loader to try reading it.
fn find_absolute_paths_in_backticks(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Triple-backtick fence: skip over its entire body.
        if i + 2 < bytes.len() && &bytes[i..i + 3] == b"```" {
            let search_from = i + 3;
            if let Some(rel) = body[search_from..].find("```") {
                i = search_from + rel + 3;
            } else {
                // Unterminated fence — stop scanning; no reliable way
                // to tell example text from prose past this point.
                break;
            }
            continue;
        }
        if bytes[i] == b'`' {
            let end = body[i + 1..].find('`').map(|j| i + 1 + j);
            let Some(end) = end else { break };
            let inner = &body[i + 1..end];
            let trimmed = inner.trim();
            if trimmed.starts_with('/') && !trimmed.contains('\n') {
                out.push(trimmed.to_string());
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(nonce: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("kres-skills-{}-{}", nonce, std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn frontmatter_split_basic() {
        let raw = "---\nname: x\ninvocation_policy: automatic\n---\nbody here\n";
        let (fm, body) = split_frontmatter(raw);
        assert!(fm.contains("name: x"));
        assert_eq!(body, "body here\n");
    }

    #[test]
    fn frontmatter_split_no_frontmatter() {
        let raw = "no fm here\n";
        let (fm, body) = split_frontmatter(raw);
        assert_eq!(fm, "");
        assert_eq!(body, raw);
    }

    #[test]
    fn parse_frontmatter_fields() {
        let fm = "name: kernel\ndescription: kernel stuff\ninvocation_policy: automatic\n";
        let m = parse_frontmatter(fm);
        assert_eq!(m.get("name"), Some(&"kernel".to_string()));
        assert_eq!(m.get("invocation_policy"), Some(&"automatic".to_string()));
    }

    #[test]
    fn backticks_absolute_paths() {
        let body = "see `/home/user/x.md` or `relative.md` or `/tmp/y.md` and stuff";
        let p = find_absolute_paths_in_backticks(body);
        assert_eq!(p, vec!["/home/user/x.md", "/tmp/y.md"]);
    }

    #[test]
    fn triple_backtick_fences_are_skipped() {
        let body = "pre `/real/path.md` then:\n```\nexample `/dangerous/example.md`\n```\nafter `/real2.md`";
        let p = find_absolute_paths_in_backticks(body);
        assert_eq!(p, vec!["/real/path.md", "/real2.md"]);
    }

    #[test]
    fn skill_from_string_minimal() {
        let raw = "---\nname: mini\n---\nbody";
        let s = Skill::from_str_with_stem(raw, "stem".to_string()).unwrap();
        assert_eq!(s.name, "mini");
        assert_eq!(s.invocation_policy, InvocationPolicy::Manual);
        assert_eq!(s.content, "body");
        assert!(s.files.is_empty());
    }

    #[test]
    fn skill_from_string_auto_and_references() {
        let dir = tmpdir("ref");
        let referenced = dir.join("referenced.md");
        std::fs::write(&referenced, "hello").unwrap();
        let body = format!(
            "---\nname: r\ninvocation_policy: automatic\n---\nsee `{}`",
            referenced.display()
        );
        let s = Skill::from_str_with_stem(&body, "fallback".into()).unwrap();
        assert_eq!(s.invocation_policy, InvocationPolicy::Automatic);
        assert!(s.files.contains_key(&referenced.display().to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_dir_filters_md_and_returns_map() {
        let dir = tmpdir("dir");
        std::fs::write(dir.join("a.md"), "---\nname: a\n---\nbody a").unwrap();
        std::fs::write(dir.join("b.md"), "---\nname: b\n---\nbody b").unwrap();
        std::fs::write(dir.join("c.txt"), "ignored").unwrap();
        let skills = Skills::load_dir(&dir).unwrap();
        assert_eq!(skills.items.len(), 2);
        assert!(skills.items.contains_key("a"));
        assert!(skills.items.contains_key("b"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_dir_returns_empty() {
        let p = PathBuf::from("/tmp/kres-skills-absolutely-nope");
        let s = Skills::load_dir(&p).unwrap();
        assert!(s.items.is_empty());
    }

    #[test]
    fn auto_loaded_picks_automatic_only() {
        let dir = tmpdir("auto");
        std::fs::write(
            dir.join("a.md"),
            "---\nname: a\ninvocation_policy: automatic\n---\nA",
        )
        .unwrap();
        std::fs::write(
            dir.join("b.md"),
            "---\nname: b\ninvocation_policy: manual\n---\nB",
        )
        .unwrap();
        let skills = Skills::load_dir(&dir).unwrap();
        let auto = skills.auto_loaded();
        assert_eq!(auto.len(), 1);
        assert_eq!(auto[0].name, "a");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auto_loaded_for_workspace_picks_detected_knowledge_only() {
        let dir = tmpdir("workspace-auto");
        std::fs::write(
            dir.join("kernel.md"),
            "---\nname: kernel\ninvocation_policy: automatic\n---\nK",
        )
        .unwrap();
        std::fs::write(
            dir.join("systemd.md"),
            "---\nname: systemd\ninvocation_policy: automatic\n---\nS",
        )
        .unwrap();
        let skills = Skills::load_dir(&dir).unwrap();
        let profile = WorkspaceProfile {
            kind: crate::workspace::WorkspaceKind::Systemd,
            build_system: crate::workspace::BuildSystem::Meson,
            knowledge_skills: vec!["systemd"],
        };

        let auto = skills.auto_loaded_for_workspace(&profile);

        assert_eq!(auto.len(), 1);
        assert_eq!(auto[0].name, "systemd");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_auto_for_workspace_does_not_parse_unselected_skills() {
        let dir = tmpdir("workspace-load-auto");
        std::fs::write(
            dir.join("systemd.md"),
            "---\nname: systemd\ninvocation_policy: automatic\n---\nS",
        )
        .unwrap();
        let unreadable = dir.join("kernel.md");
        std::fs::write(&unreadable, "---\nname: kernel\n---\nK").unwrap();
        std::fs::remove_file(&unreadable).unwrap();
        let profile = WorkspaceProfile {
            kind: crate::workspace::WorkspaceKind::Systemd,
            build_system: crate::workspace::BuildSystem::Meson,
            knowledge_skills: vec!["systemd"],
        };

        let (skills, warnings) = Skills::load_auto_for_workspace(&dir, &profile).unwrap();

        assert!(warnings.is_empty());
        assert!(skills.items.contains_key("systemd"));
        assert!(!skills.items.contains_key("kernel"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_auto_for_workspace_rejects_frontmatter_name_mismatch() {
        let dir = tmpdir("workspace-load-auto-name-mismatch");
        std::fs::write(
            dir.join("systemd.md"),
            "---\nname: kernel\ninvocation_policy: automatic\n---\nS",
        )
        .unwrap();
        let profile = WorkspaceProfile {
            kind: crate::workspace::WorkspaceKind::Systemd,
            build_system: crate::workspace::BuildSystem::Meson,
            knowledge_skills: vec!["systemd"],
        };

        let (skills, warnings) = Skills::load_auto_for_workspace(&dir, &profile).unwrap();

        assert!(skills.items.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("does not match requested skill 'systemd'"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn to_prompt_value_round_trip() {
        let dir = tmpdir("pv");
        std::fs::write(
            dir.join("a.md"),
            "---\nname: a\ninvocation_policy: automatic\n---\nbody A",
        )
        .unwrap();
        let skills = Skills::load_dir(&dir).unwrap();
        let auto = skills.auto_loaded();
        let v = skills.to_prompt_value(&auto);
        assert!(v.is_object());
        let a = v.get("a").unwrap();
        assert_eq!(a.get("content").unwrap(), "body A");
        std::fs::remove_dir_all(&dir).ok();
    }
}
