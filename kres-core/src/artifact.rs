use std::path::{Path, PathBuf};

pub const AUTO_GENERATED_FIX_NAME: &str = "auto-generated-fix.diff";
pub const AUTO_GENERATED_FIX_LINK: &str = "[auto-generated-fix.diff](auto-generated-fix.diff)";
pub const SUMMARY_CROSS_LINK: &str = "[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)";

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
    ensure_artifact_dir_files(dir)?;
    let metadata_path = dir.join("metadata.yaml");
    let metadata = std::fs::read_to_string(&metadata_path)?;
    if !metadata
        .lines()
        .any(|l| l.trim_start().starts_with("auto_generated_fix:"))
    {
        let mut updated = metadata.trim_end().to_string();
        updated.push('\n');
        updated.push_str("auto_generated_fix: auto-generated-fix.diff\n");
        std::fs::write(&metadata_path, updated)?;
    }

    let summary_path = dir.join("summary.md");
    if summary_path.exists() {
        let summary = std::fs::read_to_string(&summary_path)?;
        if !summary.contains(AUTO_GENERATED_FIX_LINK) {
            let updated = if let Some(pos) = summary.find(SUMMARY_CROSS_LINK) {
                let end = pos + SUMMARY_CROSS_LINK.len();
                format!(
                    "{} | {}{}",
                    &summary[..end],
                    AUTO_GENERATED_FIX_LINK,
                    &summary[end..]
                )
            } else {
                format!("{AUTO_GENERATED_FIX_LINK}\n\n{summary}")
            };
            std::fs::write(&summary_path, updated)?;
        }
    }
    Ok(())
}

pub fn patch_file_matches_head(dir: &Path, head_sha: &str) -> std::io::Result<bool> {
    let patch = dir.join(AUTO_GENERATED_FIX_NAME);
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
            std::fs::read_to_string(dir.join("metadata.yaml"))
                .unwrap()
                .matches("auto_generated_fix:")
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("summary.md"))
                .unwrap()
                .matches(AUTO_GENERATED_FIX_LINK)
                .count(),
            1
        );
    }
}
