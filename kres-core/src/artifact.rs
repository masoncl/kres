use std::path::{Path, PathBuf};

pub const AUTO_GENERATED_FIX_NAME: &str = "auto-generated-fix.diff";
pub const AUTO_GENERATED_FIX_LINK: &str = "[auto-generated-fix.diff](auto-generated-fix.diff)";
pub const SUMMARY_CROSS_LINK: &str = "[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)";

pub fn auto_generated_fix_name(index: u32) -> String {
    if index <= 1 {
        AUTO_GENERATED_FIX_NAME.to_string()
    } else {
        format!("auto-generated-fix-{index}.diff")
    }
}

pub fn auto_generated_fix_link(index: u32) -> String {
    let name = auto_generated_fix_name(index);
    format!("[{name}]({name})")
}

pub fn ensure_artifact_dir_files(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let id = artifact_id_for_dir(dir);
    let metadata = dir.join("metadata.yaml");
    if !metadata.exists() {
        std::fs::write(
            &metadata,
            format!("id: {}\nstatus: active\n", yaml_single_quote(&id)),
        )?;
    }
    let finding = dir.join("FINDING.md");
    if !finding.exists() {
        std::fs::write(&finding, format!("# {id}\n\n**Status:** active\n"))?;
    }
    let summary = dir.join("summary.md");
    if !summary.exists() {
        std::fs::write(&summary, "# Summary\n")?;
    }
    Ok(())
}

pub fn set_finding_status_files(finding_dir: &Path, status: &str) -> std::io::Result<Vec<PathBuf>> {
    if !matches!(status, "invalidated" | "unconfirmed") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsupported finding status: {status}"),
        ));
    }
    ensure_artifact_dir_files(finding_dir)?;
    let metadata = finding_dir.join("metadata.yaml");
    let finding = finding_dir.join("FINDING.md");

    let metadata_body = std::fs::read_to_string(&metadata)?;
    let mut saw_status = false;
    let mut metadata_lines: Vec<String> = metadata_body
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("status:") {
                saw_status = true;
                let indent_len = line.len() - line.trim_start().len();
                format!("{}status: {status}", &line[..indent_len])
            } else {
                line.to_string()
            }
        })
        .collect();
    if !saw_status {
        metadata_lines.push(format!("status: {status}"));
    }
    std::fs::write(
        &metadata,
        finish_lines(metadata_lines, metadata_body.ends_with('\n')),
    )?;

    let finding_body = std::fs::read_to_string(&finding)?;
    let mut saw_status = false;
    let mut finding_lines: Vec<String> = finding_body
        .lines()
        .map(|line| {
            if line.starts_with("**Status:**") {
                saw_status = true;
                format!("**Status:** {status}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !saw_status {
        finding_lines.push(format!("**Status:** {status}"));
    }
    std::fs::write(
        &finding,
        finish_lines(finding_lines, finding_body.ends_with('\n')),
    )?;

    Ok(vec![metadata, finding])
}

pub fn record_auto_generated_fix(dir: &Path) -> std::io::Result<()> {
    record_auto_generated_fix_named(dir, AUTO_GENERATED_FIX_NAME)
}

pub fn record_auto_generated_fix_named(dir: &Path, fix_name: &str) -> std::io::Result<()> {
    validate_auto_generated_fix_name(fix_name)?;
    ensure_artifact_dir_files(dir)?;
    let metadata_path = dir.join("metadata.yaml");
    let metadata = std::fs::read_to_string(&metadata_path)?;
    let updated = metadata_with_auto_generated_fix(&metadata, fix_name);
    if updated != metadata {
        std::fs::write(&metadata_path, updated)?;
    }

    let summary_path = dir.join("summary.md");
    if summary_path.exists() {
        let summary = std::fs::read_to_string(&summary_path)?;
        let link = format!("[{fix_name}]({fix_name})");
        if !summary.contains(&link) {
            let updated = summary_with_auto_generated_fix_link(&summary, &link);
            std::fs::write(&summary_path, updated)?;
        }
    }
    Ok(())
}

pub fn patch_file_matches_head(dir: &Path, head_sha: &str) -> std::io::Result<bool> {
    patch_file_matches_head_named(dir, AUTO_GENERATED_FIX_NAME, head_sha)
}

pub fn patch_file_matches_head_named(
    dir: &Path,
    fix_name: &str,
    head_sha: &str,
) -> std::io::Result<bool> {
    validate_auto_generated_fix_name(fix_name)?;
    let patch = dir.join(fix_name);
    let existing = std::fs::read_to_string(patch)?;
    Ok(existing
        .lines()
        .next()
        .is_some_and(|l| l.starts_with(&format!("From {head_sha} "))))
}

fn finish_lines(lines: Vec<String>, trailing_newline: bool) -> String {
    let mut s = lines.join("\n");
    if trailing_newline {
        s.push('\n');
    }
    s
}

fn metadata_with_auto_generated_fix(metadata: &str, fix_name: &str) -> String {
    let mut kept = Vec::new();
    let mut fixes = Vec::<String>::new();
    let mut lines = metadata.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let top_level = line == trimmed;
        if top_level {
            if let Some(value) = trimmed.strip_prefix("auto_generated_fix:") {
                push_unique(&mut fixes, value.trim());
                continue;
            }
        }
        if top_level && trimmed == "auto_generated_fixes:" {
            while let Some(next) = lines.peek() {
                let next_trimmed = next.trim_start();
                if let Some(value) = next_trimmed.strip_prefix("- ") {
                    push_unique(&mut fixes, value.trim());
                    lines.next();
                } else if next_trimmed.is_empty() {
                    kept.push((*next).to_string());
                    lines.next();
                } else {
                    break;
                }
            }
            continue;
        }
        kept.push(line.to_string());
    }
    push_unique(&mut fixes, fix_name);

    let mut updated = kept.join("\n").trim_end().to_string();
    if !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str("auto_generated_fixes:\n");
    for fix in fixes {
        updated.push_str("- ");
        updated.push_str(&fix);
        updated.push('\n');
    }
    updated
}

fn validate_auto_generated_fix_name(fix_name: &str) -> std::io::Result<()> {
    let valid = fix_name == AUTO_GENERATED_FIX_NAME
        || fix_name
            .strip_prefix("auto-generated-fix-")
            .and_then(|suffix| suffix.strip_suffix(".diff"))
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|index| index >= 2);
    if valid {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid auto-generated fix filename: {fix_name}"),
        ))
    }
}

fn summary_with_auto_generated_fix_link(summary: &str, link: &str) -> String {
    let mut lines: Vec<String> = summary.lines().map(str::to_string).collect();
    if let Some(line) = lines
        .iter_mut()
        .find(|line| line.contains(SUMMARY_CROSS_LINK))
    {
        line.push_str(" | ");
        line.push_str(link);
        let mut updated = lines.join("\n");
        if summary.ends_with('\n') {
            updated.push('\n');
        }
        return updated;
    }
    format!("{link}\n\n{summary}")
}

fn push_unique(items: &mut Vec<String>, value: &str) {
    if value.is_empty() || items.iter().any(|item| item == value) {
        return;
    }
    items.push(value.to_string());
}

fn artifact_id_for_dir(dir: &Path) -> String {
    dir.file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("kres-result")
        .to_string()
}

fn yaml_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_artifact_dir_files_quotes_generated_metadata_id() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fix: psp's bug #1");

        ensure_artifact_dir_files(&dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("metadata.yaml")).unwrap(),
            "id: 'fix: psp''s bug #1'\nstatus: active\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("FINDING.md")).unwrap(),
            "# fix: psp's bug #1\n\n**Status:** active\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("summary.md")).unwrap(),
            "# Summary\n"
        );
    }

    #[test]
    fn set_finding_status_updates_status_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(
            dir.join("FINDING.md"),
            "# Finding\n\n**Status:** active\n\nbody\n",
        )
        .unwrap();

        let files = set_finding_status_files(dir, "unconfirmed").unwrap();

        assert_eq!(files.len(), 2);
        assert_eq!(
            std::fs::read_to_string(dir.join("metadata.yaml")).unwrap(),
            "id: F1\nstatus: unconfirmed\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("FINDING.md")).unwrap(),
            "# Finding\n\n**Status:** unconfirmed\n\nbody\n"
        );
    }

    #[test]
    fn record_auto_generated_fix_updates_metadata_and_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(
            dir.join("summary.md"),
            "[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)\n\nbody\n",
        )
        .unwrap();

        record_auto_generated_fix(dir).unwrap();
        record_auto_generated_fix(dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("metadata.yaml")).unwrap(),
            "id: F1\nstatus: active\nauto_generated_fixes:\n- auto-generated-fix.diff\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("summary.md"))
                .unwrap()
                .matches(AUTO_GENERATED_FIX_LINK)
                .count(),
            1
        );
    }

    #[test]
    fn record_auto_generated_fix_named_appends_series_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(
            dir.join("summary.md"),
            "[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)\n\nbody\n",
        )
        .unwrap();

        record_auto_generated_fix_named(dir, "auto-generated-fix.diff").unwrap();
        record_auto_generated_fix_named(dir, "auto-generated-fix-2.diff").unwrap();
        record_auto_generated_fix_named(dir, "auto-generated-fix-2.diff").unwrap();

        let metadata = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        assert!(metadata.contains("auto_generated_fixes:\n"));
        assert!(metadata.contains("- auto-generated-fix.diff\n"));
        assert!(metadata.contains("- auto-generated-fix-2.diff\n"));
        assert_eq!(metadata.matches("- auto-generated-fix-2.diff").count(), 1);
        let summary = std::fs::read_to_string(dir.join("summary.md")).unwrap();
        assert!(summary.contains("[auto-generated-fix.diff](auto-generated-fix.diff)"));
        assert!(summary.contains("[auto-generated-fix-2.diff](auto-generated-fix-2.diff)"));
        assert!(summary.contains(
            "[metadata.yaml](metadata.yaml) | [auto-generated-fix.diff](auto-generated-fix.diff) | [auto-generated-fix-2.diff](auto-generated-fix-2.diff)"
        ));
    }

    #[test]
    fn record_auto_generated_fix_named_rejects_unexpected_names() {
        let tmp = tempfile::tempdir().unwrap();
        let err = record_auto_generated_fix_named(tmp.path(), "../outside.diff").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn metadata_update_only_rewrites_top_level_fix_keys() {
        let metadata = "\
id: F1
nested:
  auto_generated_fix: keep-me.diff
  auto_generated_fixes:
    - keep-me-2.diff
auto_generated_fixes:
  - auto-generated-fix.diff
status: active
";

        let updated = metadata_with_auto_generated_fix(metadata, "auto-generated-fix-2.diff");

        assert!(updated.contains("  auto_generated_fix: keep-me.diff\n"));
        assert!(updated.contains("  auto_generated_fixes:\n"));
        assert!(updated.contains("    - keep-me-2.diff\n"));
        assert!(updated.contains("status: active\n"));
        assert!(updated.contains("auto_generated_fixes:\n"));
        assert!(updated.contains("- auto-generated-fix.diff\n"));
        assert!(updated.contains("- auto-generated-fix-2.diff\n"));
    }
}
