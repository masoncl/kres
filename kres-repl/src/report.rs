//! Markdown report writer.
//!
//! Produces a human-friendly report of the current findings list.
//! Groups by severity (high → medium → low), renders
//! mechanism_detail / fix_sketch / open_questions when present.

use std::io::Write;

use kres_core::findings::{Finding, Severity};

pub fn render_findings_markdown(findings: &[Finding]) -> String {
    let mut out = String::new();
    out.push_str("# kres findings report\n\n");
    let timestamp = chrono::Utc::now().to_rfc3339();
    out.push_str(&format!("_generated: {}_\n\n", timestamp));
    if findings.is_empty() {
        out.push_str("(no findings yet)\n");
        return out;
    }
    out.push_str(&format!("{} finding(s):\n", findings.len()));
    out.push_str(&severity_histogram(findings));
    out.push('\n');

    for sev in [Severity::High, Severity::Medium, Severity::Low] {
        let bucket: Vec<&Finding> = findings.iter().filter(|f| f.severity == sev).collect();
        if bucket.is_empty() {
            continue;
        }
        out.push_str(&format!("## {:?}\n\n", sev));
        for f in bucket {
            render_finding(&mut out, f);
        }
    }
    out
}

fn severity_histogram(findings: &[Finding]) -> String {
    let (h, m, l) = findings
        .iter()
        .fold((0, 0, 0), |(h, m, l), f| match f.severity {
            Severity::High => (h + 1, m, l),
            Severity::Medium => (h, m + 1, l),
            Severity::Low => (h, m, l + 1),
        });
    format!("- {} high, {} medium, {} low\n", h, m, l)
}

fn render_finding(out: &mut String, f: &Finding) {
    out.push_str(&format!("### `{}` — {}\n\n", f.id, f.title));
    out.push_str(&format!("**Severity:** {:?}  \n", f.severity));
    if !f.related_finding_ids.is_empty() {
        out.push_str("**Related:** ");
        out.push_str(
            &f.related_finding_ids
                .iter()
                .map(|id| format!("`{id}`"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push('\n');
    }
    out.push_str("\n**Summary**\n\n");
    out.push_str(&f.summary);
    out.push_str("\n\n");

    if let Some(ref md) = f.mechanism_detail {
        if !md.is_empty() {
            out.push_str("**Mechanism**\n\n");
            out.push_str(md);
            out.push_str("\n\n");
        }
    }

    out.push_str("**Reproducer**\n\n");
    out.push_str(&f.reproducer_sketch);
    out.push_str("\n\n**Impact**\n\n");
    out.push_str(&f.impact);
    out.push_str("\n\n");

    if let Some(ref fx) = f.fix_sketch {
        if !fx.is_empty() {
            out.push_str("**Fix sketch**\n\n");
            out.push_str(fx);
            out.push_str("\n\n");
        }
    }

    if !f.open_questions.is_empty() {
        out.push_str("**Open questions**\n\n");
        for q in &f.open_questions {
            out.push_str(&format!("- {}\n", q));
        }
        out.push('\n');
    }

    if !f.relevant_symbols.is_empty() {
        out.push_str("**Relevant symbols**\n\n");
        for s in &f.relevant_symbols {
            out.push_str(&format!("- `{}` at `{}:{}`\n", s.name, s.filename, s.line));
        }
        out.push('\n');
    }
    out.push_str("---\n\n");
}

pub fn write_findings_to_file(findings: &[Finding], path: &std::path::Path) -> std::io::Result<()> {
    // Create the parent dir so a fresh `/report out/foo.md` call
    // doesn't fail purely on a missing directory.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let body = render_findings_markdown(findings);
    let mut f = std::fs::File::create(path)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

/// §26: per-task append. Each reaped task writes a fresh section
/// headed `## [type] name` with an ISO-8601 timestamp and the
/// task's analysis body. Matches
/// (/+): the report accretes chronologically,
/// the findings sidebar lives in the separate JSON store.
///
/// Creates the file if missing; appends otherwise. Newlines are
/// inserted so repeated appends stay visually separated.
pub fn append_task_section(
    path: &std::path::Path,
    task_label: &str,
    analysis: &str,
) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let ts = chrono::Utc::now().to_rfc3339();
    let body = format!("\n## {task_label}\n\n_run: {ts}_\n\n{analysis}\n\n---\n");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kres_core::findings::{Finding, Severity, Status};

    fn finding(id: &str, sev: Severity) -> Finding {
        Finding {
            id: id.to_string(),
            title: format!("finding {id}"),
            severity: sev,
            status: Status::Active,
            relevant_symbols: vec![],
            relevant_file_sections: vec![],
            summary: "s".into(),
            reproducer_sketch: "r".into(),
            impact: "i".into(),
            mechanism_detail: Some("md".into()),
            fix_sketch: Some("cache a bool".into()),
            open_questions: vec!["is x verified?".into()],
            first_seen_task: None,
            last_updated_task: None,
            related_finding_ids: vec!["other".into()],
            reactivate: false,
            details: vec![],
            introduced_by: None,
            first_seen_at: None,
        }
    }

    #[test]
    fn empty_findings_render_stub() {
        let md = render_findings_markdown(&[]);
        assert!(md.contains("no findings yet"));
    }

    #[test]
    fn groups_by_severity_in_order() {
        let findings = vec![
            finding("a", Severity::Low),
            finding("b", Severity::High),
            finding("c", Severity::Medium),
        ];
        let md = render_findings_markdown(&findings);
        let high_pos = md.find("## High").unwrap();
        let med_pos = md.find("## Medium").unwrap();
        let low_pos = md.find("## Low").unwrap();
        assert!(high_pos < med_pos);
        assert!(med_pos < low_pos);
    }

    #[test]
    fn includes_optional_sections_when_present() {
        let f = finding("x", Severity::High);
        let md = render_findings_markdown(&[f]);
        assert!(md.contains("**Mechanism**"));
        assert!(md.contains("cache a bool"));
        assert!(md.contains("is x verified?"));
        assert!(md.contains("Related:"));
    }

    #[test]
    fn histogram_counts_correctly() {
        let f = vec![
            finding("a", Severity::High),
            finding("b", Severity::High),
            finding("c", Severity::Medium),
        ];
        let md = render_findings_markdown(&f);
        assert!(md.contains("2 high"));
        assert!(md.contains("1 medium"));
    }
}
