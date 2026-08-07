use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use gix::diff::blob::platform::prepare_diff::Operation;
use gix::diff::blob::unified_diff::{ContextSize, NewlineSeparator};
use gix::diff::blob::UnifiedDiff;
use gix::object::tree::diff::Change;
use serde::{Deserialize, Serialize};

const MAJOR_EXTERNAL_RISK: u8 = 80;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RecentCommit {
    pub id: String,
    pub committed_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FunctionRisk {
    pub name: String,
    pub risk_rating: u8,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ExternalFunctionRisk {
    pub name: String,
    pub file: String,
    pub risk_rating: u8,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ChangeSurveyReport {
    pub baseline: String,
    pub head: String,
    #[serde(default)]
    pub target_function_risks: Vec<FunctionRisk>,
    #[serde(default)]
    pub external_major_risks: Vec<ExternalFunctionRisk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AggregateTargetDiff {
    pub baseline: String,
    pub head: String,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangeSurveyPrompt {
    pub cached_prefix: String,
    pub tail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChangeSurveyDiffChunk<'a> {
    pub text: &'a str,
    pub index: usize,
    pub count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChangeSurveySourceChunk<'a> {
    pub text: &'a str,
    pub index: usize,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedDiffChunk {
    pub text: String,
    pub source_start: usize,
    pub source_end: usize,
}

#[derive(Debug, Deserialize)]
struct InferenceRiskSurvey {
    #[serde(default)]
    target_function_risks: Vec<FunctionRisk>,
    #[serde(default)]
    external_major_risks: Vec<ExternalFunctionRisk>,
}

pub(crate) fn parse_inference_risks(
    text: &str,
    baseline: &str,
    head: &str,
) -> Result<ChangeSurveyReport> {
    let mut parsed: InferenceRiskSurvey =
        serde_json::from_str(text.trim()).context("change survey response is not raw JSON")?;
    for risk in &parsed.target_function_risks {
        validate_risk(&risk.name, risk.risk_rating)?;
    }
    let target_names: BTreeSet<&str> = parsed
        .target_function_risks
        .iter()
        .map(|risk| risk.name.as_str())
        .collect();
    if target_names.len() != parsed.target_function_risks.len() {
        bail!("change survey duplicated a target function risk");
    }
    for risk in &parsed.external_major_risks {
        validate_risk(&risk.name, risk.risk_rating)?;
        if risk.file.trim().is_empty() {
            bail!("change survey returned an external risk without a file");
        }
    }
    parsed
        .external_major_risks
        .retain(|risk| risk.risk_rating >= MAJOR_EXTERNAL_RISK);
    let external_keys: BTreeSet<(&str, &str)> = parsed
        .external_major_risks
        .iter()
        .map(|risk| (risk.name.as_str(), risk.file.as_str()))
        .collect();
    if external_keys.len() != parsed.external_major_risks.len() {
        bail!("change survey duplicated an external function risk");
    }
    Ok(ChangeSurveyReport {
        baseline: baseline.to_string(),
        head: head.to_string(),
        target_function_risks: parsed.target_function_risks,
        external_major_risks: parsed.external_major_risks,
    })
}

pub(crate) fn validate_function_coverage(
    survey: &ChangeSurveyReport,
    expected: &BTreeSet<String>,
) -> Result<()> {
    validate_function_subset(survey, expected)?;
    let actual: BTreeSet<&str> = survey
        .target_function_risks
        .iter()
        .map(|risk| risk.name.as_str())
        .collect();
    if actual.len() != survey.target_function_risks.len()
        || actual != expected.iter().map(String::as_str).collect()
    {
        bail!("change survey did not rate every target function exactly once");
    }
    Ok(())
}

/// Accept a pre-file-survey report only when it already covers the authoritative
/// function inventory exactly. Missing functions are not evidence of zero risk;
/// the caller must perform a corrective inference pass with the authoritative
/// names instead of manufacturing ratings in Rust.
pub(crate) fn complete_function_coverage(
    survey: ChangeSurveyReport,
    expected: &BTreeSet<String>,
) -> Result<ChangeSurveyReport> {
    validate_function_coverage(&survey, expected)?;
    Ok(survey)
}

/// Union the per-partition change-survey reports into one.
///
/// Each partition rates the functions it could see in its own source
/// scope, so the whole-file answer is the union — not something a
/// model needs to reassemble. It used to be: the partitioned path
/// ended in a "reduction" inference call that was handed every
/// partial report and asked to re-emit a complete, exactly-once
/// roster. On kernel/sched/fair.c (429,745 source bytes, 521
/// functions) that call failed twice and killed the review bootstrap
/// — once by inventing `__account_cfs_rq_runtime_placeholder`, a name
/// that appears nowhere in the file, and once by simply missing
/// functions. Reassembling a roster Rust already holds is mechanical
/// work, and a model asked for 521 exact entries will pad or drop.
///
/// Highest rating wins per function, with that entry's reason. A
/// partition that could not see a function does not get to outvote
/// one that could, and no rating is manufactured here: a function no
/// partition rated is simply absent, which the caller must resolve
/// with a corrective pass rather than by assuming zero risk.
pub(crate) fn merge_change_survey_reports(
    baseline: &str,
    head: &str,
    reports: Vec<ChangeSurveyReport>,
) -> ChangeSurveyReport {
    let mut target: BTreeMap<String, FunctionRisk> = BTreeMap::new();
    let mut external: BTreeMap<(String, String), ExternalFunctionRisk> = BTreeMap::new();
    for report in reports {
        for risk in report.target_function_risks {
            match target.get(&risk.name) {
                Some(existing) if existing.risk_rating >= risk.risk_rating => {}
                _ => {
                    target.insert(risk.name.clone(), risk);
                }
            }
        }
        for risk in report.external_major_risks {
            let key = (risk.name.clone(), risk.file.clone());
            match external.get(&key) {
                Some(existing) if existing.risk_rating >= risk.risk_rating => {}
                _ => {
                    external.insert(key, risk);
                }
            }
        }
    }
    ChangeSurveyReport {
        baseline: baseline.to_string(),
        head: head.to_string(),
        target_function_risks: target.into_values().collect(),
        external_major_risks: external.into_values().collect(),
    }
}

/// Authoritative names no partition rated.
pub(crate) fn unrated_functions(
    survey: &ChangeSurveyReport,
    expected: &BTreeSet<String>,
) -> BTreeSet<String> {
    let rated: BTreeSet<&str> = survey
        .target_function_risks
        .iter()
        .map(|risk| risk.name.as_str())
        .collect();
    expected
        .iter()
        .filter(|name| !rated.contains(name.as_str()))
        .cloned()
        .collect()
}

pub(crate) fn validate_function_subset(
    survey: &ChangeSurveyReport,
    expected: &BTreeSet<String>,
) -> Result<()> {
    if let Some(risk) = survey
        .target_function_risks
        .iter()
        .find(|risk| !expected.contains(&risk.name))
    {
        bail!(
            "change survey reported unknown target function {}",
            risk.name
        );
    }
    // External identities are (file, name), not bare names. Static helpers
    // routinely share names across kernel translation units. Whether a call
    // in the target resolves to the external entry is decided later from the
    // target inventory; a same-named target definition does not make the
    // external risk itself malformed.
    Ok(())
}

fn validate_risk(name: &str, risk: u8) -> Result<()> {
    if name.trim().is_empty() {
        bail!("change survey returned an empty function name");
    }
    if risk > 100 {
        bail!("change survey risk for {name} exceeds 100");
    }
    Ok(())
}

pub(crate) fn change_survey_prompt(
    target: &str,
    target_source: &str,
    window: &AggregateTargetDiff,
    expected_functions: Option<&BTreeSet<String>>,
) -> ChangeSurveyPrompt {
    let expected = expected_functions.map_or_else(String::new, |functions| {
        format!(
            "\nThe authoritative file_survey function set is: {}. The target_function_risks names must match this set exactly, with no duplicates.\n",
            serde_json::to_string(functions).expect("serializing function names cannot fail")
        )
    });
    let cached_prefix = format!(
        "Survey the net changes from the last six months for review risk. The target file is {target}.\n\
         Emit exactly one \
         target_function_risks entry for every function defined in that target source, even when \
         the net diff provides little evidence of risk for a function. Do not emit target_function_risks \
         for functions from any other file. A rating estimates \
         the likelihood that the current six-month net change contains a correctness bug in that function, from 0 to 100. \
         Judge the final code represented by the net diff; do not preserve risks that later changes in the same diff fix.\n\
         You may separately flag functions outside the target file only when their risk is major \
         (80-100) and the target-file diff provides concrete interaction evidence. These are candidates for later research, \
         not target-file ratings. Do not decide whether the target interacts with them; the file \
         survey will make that decision from its structural call inventory.\n\
         Return exactly one raw JSON object with this schema:\n\
         {{\"target_function_risks\":[{{\"name\":string,\"risk_rating\":integer,\"reason\":string}}],\
         \"external_major_risks\":[{{\"name\":string,\"file\":string,\"risk_rating\":integer,\"reason\":string}}]}}\n\
         A function with little relevant evidence still needs a low, evidence-based rating; only external_major_risks may be empty. Keep each reason to at most 12 words; use \"No net-diff evidence.\" for unchanged functions.{expected}\
         No markdown or prose outside JSON.\n\n\
         CURRENT TARGET FILE ({target}):\n{target_source}\n\n--- END CURRENT TARGET FILE ---\n\n"
    );
    let tail = format!(
        "BASELINE: {}\nHEAD: {}\n\nSIX-MONTH TARGET-FILE DIFF:\n{}",
        window.baseline, window.head, window.diff
    );
    ChangeSurveyPrompt {
        cached_prefix,
        tail,
    }
}

pub(crate) fn change_survey_chunk_prompt(
    target: &str,
    window: &AggregateTargetDiff,
    expected_functions: Option<&BTreeSet<String>>,
    source_chunk: Option<ChangeSurveySourceChunk<'_>>,
    chunk: Option<ChangeSurveyDiffChunk<'_>>,
) -> ChangeSurveyPrompt {
    let function_scope = expected_functions.map_or_else(
        || {
            "Emit sparse target_function_risks only for functions defined in the target file for which this source/diff pair provides concrete risk evidence; an empty array is valid.".to_string()
        },
        |functions| {
            format!(
                "The authoritative target-file function set is {}. Emit target_function_risks only for functions from this set for which this source/diff pair provides concrete risk evidence; an empty array is valid.",
                serde_json::to_string(functions)
                    .expect("serializing function names cannot fail")
            )
        },
    );
    let source_section = source_chunk.map_or_else(
        || "No current-source bytes are assigned to this partition.".to_string(),
        |source_chunk| {
            format!(
                "CURRENT TARGET FILE CHUNK ({target}, {}/{}):\n{}\n\n--- END CURRENT TARGET FILE CHUNK ---",
                source_chunk.index + 1,
                source_chunk.count,
                source_chunk.text,
            )
        },
    );
    let cached_prefix = format!(
        "Survey one chunk of the target file's large six-month net diff for review risk. The target file is {target}.\n\
         {function_scope} A rating estimates the likelihood that the final six-month net change contains \
         a correctness bug in that function, from 0 to 100. Rust will combine every chunk \
         with a final inference pass. Report the evidence in this source/diff scope without assuming it is the \
         final rating; the reducer will reconcile fixes or contradictions from other chunks.\n\
         You may separately flag functions outside the target file only when their risk is major \
         (80-100) and this scope provides a concrete reason. Do not decide whether the target \
         interacts with them. Return exactly one raw JSON object with this schema:\n\
         {{\"target_function_risks\":[{{\"name\":string,\"risk_rating\":integer,\"reason\":string}}],\
         \"external_major_risks\":[{{\"name\":string,\"file\":string,\"risk_rating\":integer,\"reason\":string}}]}}\n\
         Sibling calls cover every source scope against every diff chunk. Do not treat absence from this scope as evidence of absence from the file. \
         Keep each reason to at most 12 words. No markdown or prose outside JSON.\n\n\
         {source_section}\n\n",
    );
    let diff_section = chunk.map_or_else(
        || "No diff bytes are assigned to this partition.".to_string(),
        |chunk| {
            format!(
                "DIFF_CHUNK: {}/{}\n\nSIX-MONTH TARGET-FILE DIFF CHUNK:\n{}",
                chunk.index + 1,
                chunk.count,
                chunk.text,
            )
        },
    );
    let tail = format!(
        "BASELINE: {}\nHEAD: {}\n\n{}",
        window.baseline, window.head, diff_section
    );
    ChangeSurveyPrompt {
        cached_prefix,
        tail,
    }
}

/// Split a source file into ordered UTF-8/newline-aligned pieces. The returned
/// source ranges reconstruct the original text byte-for-byte; prompt labels
/// live outside `text` so no transport annotation is confused with source.
pub(crate) fn split_source_for_inference(
    source: &str,
    max_chunk_bytes: usize,
) -> Result<Vec<PreparedDiffChunk>> {
    if max_chunk_bytes == 0 {
        bail!("change survey source chunk size must be positive");
    }
    if source.is_empty() {
        return Ok(vec![PreparedDiffChunk {
            text: String::new(),
            source_start: 0,
            source_end: 0,
        }]);
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < source.len() {
        let mut end = (start + max_chunk_bytes).min(source.len());
        while end > start && !source.is_char_boundary(end) {
            end -= 1;
        }
        if end < source.len() {
            if let Some(newline) = source[start..end].rfind('\n') {
                end = start + newline + 1;
            }
        }
        if end == start {
            end = source[start..]
                .char_indices()
                .nth(1)
                .map_or(source.len(), |(offset, _)| start + offset);
        }
        chunks.push(PreparedDiffChunk {
            text: source[start..end].to_string(),
            source_start: start,
            source_end: end,
        });
        start = end;
    }
    Ok(chunks)
}

pub(crate) fn split_diff_for_inference(
    diff: &str,
    max_chunk_bytes: usize,
) -> Result<Vec<PreparedDiffChunk>> {
    if max_chunk_bytes == 0 {
        bail!("change survey diff chunk size must be positive");
    }
    if diff.is_empty() {
        return Ok(vec![PreparedDiffChunk {
            text: String::new(),
            source_start: 0,
            source_end: 0,
        }]);
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < diff.len() {
        let context = diff_chunk_context(diff, start);
        let body_budget = max_chunk_bytes
            .checked_sub(context.len())
            .filter(|budget| *budget > 0)
            .context("change survey chunk context leaves no room for diff content")?;
        let mut end = (start + body_budget).min(diff.len());
        while !diff.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            bail!("change survey diff chunk size cannot hold one character");
        }
        if end < diff.len() {
            if let Some(boundary) = diff[start..end].rfind("\n@@ ") {
                if boundary > 0 {
                    end = start + boundary + 1;
                }
            } else if let Some(boundary) = diff[start..end].rfind('\n') {
                if boundary > 0 {
                    end = start + boundary + 1;
                }
            }
        }
        let mut text = context;
        text.push_str(&diff[start..end]);
        chunks.push(PreparedDiffChunk {
            text,
            source_start: start,
            source_end: end,
        });
        start = end;
    }
    Ok(chunks)
}

fn diff_chunk_context(diff: &str, start: usize) -> String {
    if start == 0 {
        return String::new();
    }
    let first_hunk = diff.find("\n@@ ").map(|offset| offset + 1);
    let file_header = first_hunk.map_or("", |end| &diff[..end.min(start)]);
    let hunk_header = diff[..start]
        .rfind("\n@@ ")
        .map(|offset| offset + 1)
        .and_then(|offset| {
            let end = diff[offset..].find('\n').map(|end| offset + end + 1)?;
            Some(&diff[offset..end])
        })
        .unwrap_or("");
    format!(
        "{file_header}CHUNK CONTINUATION: earlier lines from this target-file diff are omitted here.\n{hunk_header}"
    )
}

pub(crate) fn recent_target_commits(
    workspace: &Path,
    target: &str,
    cutoff_seconds: i64,
) -> Result<Vec<RecentCommit>> {
    let (repo, target_path) = open_repo_and_target(workspace, target)?;
    let head = repo.head_id().context("resolving repository HEAD")?;
    let walk = repo
        .rev_walk([head.detach()])
        .sorting(gix::revision::walk::Sorting::ByCommitTimeCutoff {
            order: gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
            seconds: cutoff_seconds,
        })
        .all()
        .context("walking recent commits")?;
    let mut commits = Vec::new();
    let mut tracked_paths = BTreeSet::from([target_path]);
    for info in walk {
        let info = info.context("reading recent commit")?;
        let is_merge = info.parent_ids.len() > 1;
        let commit = info.object().context("loading recent commit")?;
        let current_tree = commit.tree().context("loading commit tree")?;
        let mut parent_trees = info
            .parent_ids
            .iter()
            .map(|parent_id| {
                repo.find_commit(*parent_id)
                    .context("loading commit parent")?
                    .tree()
                    .context("loading parent tree")
            })
            .collect::<Result<Vec<_>>>()?;
        if parent_trees.is_empty() {
            parent_trees.push(repo.empty_tree());
        }
        let mut touched = false;
        let mut renamed_sources = Vec::new();
        for parent_tree in parent_trees {
            let edge_touched = tracked_paths.iter().try_fold(false, |touched, path| {
                Ok::<_, anyhow::Error>(
                    touched
                        || entry_identity(&current_tree, path)?
                            != entry_identity(&parent_tree, path)?,
                )
            })?;
            if !edge_touched {
                continue;
            }
            touched = true;
            parent_tree
                .changes()
                .context("initializing rename-aware history diff")?
                .options(|options| {
                    options.track_rewrites(Some(Default::default()));
                })
                .for_each_to_obtain_tree(&current_tree, |change| {
                    if let Change::Rewrite {
                        source_location,
                        location,
                        copy: false,
                        ..
                    } = change
                    {
                        let destination = gix::path::from_bstr(location);
                        if tracked_paths.contains(destination.as_ref()) {
                            touched = true;
                            renamed_sources
                                .push(gix::path::from_bstr(source_location).into_owned());
                        }
                    }
                    Ok::<_, anyhow::Error>(gix::object::tree::diff::Action::Continue)
                })
                .context("following target renames")?;
        }
        tracked_paths.extend(renamed_sources);
        if touched && !is_merge {
            commits.push(RecentCommit {
                id: info.id.to_string(),
                committed_at: info.commit_time(),
            });
        }
    }
    Ok(commits)
}

pub(crate) fn aggregate_target_diff(
    workspace: &Path,
    target: &str,
    cutoff_seconds: i64,
) -> Result<AggregateTargetDiff> {
    let commits = recent_target_commits(workspace, target, cutoff_seconds)?;
    let (repo, target_path) = open_repo_and_target(workspace, target)?;
    let head_id = repo
        .head_id()
        .context("resolving repository HEAD")?
        .detach();
    let head_tree = repo
        .find_commit(head_id)
        .context("loading aggregate diff HEAD")?
        .tree()
        .context("loading aggregate diff HEAD tree")?;
    let (baseline, baseline_tree, tracked_paths) = if let Some(oldest) = commits.last() {
        let oldest_id = gix::ObjectId::from_hex(oldest.id.as_bytes())
            .context("parsing oldest target commit")?;
        let tracked_paths = target_paths_through_commit(&repo, target_path.clone(), oldest_id)?;
        let oldest_commit = repo
            .find_commit(oldest_id)
            .context("loading oldest target commit")?;
        let baseline = match oldest_commit.parent_ids().next() {
            Some(parent) => {
                let parent = parent.detach();
                let tree = repo
                    .find_commit(parent)
                    .context("loading six-month baseline commit")?
                    .tree()
                    .context("loading six-month baseline tree")?;
                (parent.to_string(), tree, tracked_paths)
            }
            None => ("EMPTY_TREE".to_string(), repo.empty_tree(), tracked_paths),
        };
        baseline
    } else {
        (
            head_id.to_string(),
            head_tree,
            BTreeSet::from([target_path.clone()]),
        )
    };
    let baseline_entry = baseline_target_entry(&baseline_tree, &tracked_paths)?;
    let diff = render_baseline_to_worktree(&repo, baseline_entry, &target_path)?;
    Ok(AggregateTargetDiff {
        baseline,
        head: format!("WORKTREE@{head_id}"),
        diff,
    })
}

#[derive(Debug)]
struct BaselineTargetEntry {
    path: PathBuf,
    id: gix::ObjectId,
    mode: u16,
    kind: gix::object::tree::EntryKind,
}

fn baseline_target_entry(
    tree: &gix::Tree<'_>,
    tracked_paths: &BTreeSet<PathBuf>,
) -> Result<Option<BaselineTargetEntry>> {
    let mut entries = Vec::new();
    for path in tracked_paths {
        let Some(entry) = tree
            .lookup_entry_by_path(path)
            .context("looking up target in six-month baseline")?
        else {
            continue;
        };
        if entry.mode().is_tree() || entry.mode().is_commit() {
            bail!("six-month baseline target {} is not a file", path.display());
        }
        entries.push(BaselineTargetEntry {
            path: path.clone(),
            id: entry.id().detach(),
            mode: entry.mode().value(),
            kind: entry.mode().kind(),
        });
    }
    match entries.len() {
        0 => Ok(None),
        1 => Ok(entries.pop()),
        count => bail!(
            "target rename history is ambiguous at the six-month baseline ({count} candidate paths)"
        ),
    }
}

fn render_baseline_to_worktree(
    repo: &gix::Repository,
    baseline: Option<BaselineTargetEntry>,
    target_path: &Path,
) -> Result<String> {
    let workdir = repo
        .workdir()
        .context("whole-file review requires a worktree")?;
    let worktree_bytes = std::fs::read(workdir.join(target_path))
        .with_context(|| format!("reading working-tree target {}", target_path.display()))?;
    let metadata = std::fs::metadata(workdir.join(target_path))
        .with_context(|| format!("reading target metadata {}", target_path.display()))?;
    let (new_mode, new_kind) = worktree_blob_mode(&metadata);
    let old_bytes = baseline
        .as_ref()
        .map(|entry| {
            repo.find_blob(entry.id)
                .context("loading six-month baseline target blob")
                .map(|blob| blob.data.clone())
        })
        .transpose()?
        .unwrap_or_default();
    let same_path = baseline
        .as_ref()
        .is_some_and(|entry| entry.path == target_path);
    let same_mode = baseline
        .as_ref()
        .is_some_and(|entry| entry.mode == new_mode);
    if same_path && same_mode && old_bytes == worktree_bytes {
        return Ok(String::new());
    }

    let old_path = baseline
        .as_ref()
        .map_or_else(|| target_path.to_path_buf(), |entry| entry.path.clone());
    let old_location = gix::path::into_bstr(old_path.clone());
    let new_location = gix::path::into_bstr(target_path.to_path_buf());
    let mut resource_cache = repo
        .diff_resource_cache(
            gix::diff::blob::pipeline::Mode::ToGit,
            gix::diff::blob::pipeline::WorktreeRoots {
                old_root: None,
                new_root: Some(workdir.to_path_buf()),
            },
        )
        .context("creating worktree target diff resource cache")?;
    let null_id = repo.object_hash().null();
    resource_cache
        .set_resource(
            baseline.as_ref().map_or(null_id, |entry| entry.id),
            baseline.as_ref().map_or(new_kind, |entry| entry.kind),
            old_location.as_ref(),
            gix::diff::blob::ResourceKind::OldOrSource,
            &repo.objects,
        )
        .context("loading baseline target diff resource")?;
    resource_cache
        .set_resource(
            null_id,
            new_kind,
            new_location.as_ref(),
            gix::diff::blob::ResourceKind::NewOrDestination,
            &repo.objects,
        )
        .context("loading working-tree target diff resource")?;
    resource_cache
        .options
        .skip_internal_diff_if_external_is_configured = false;
    let prepared = resource_cache
        .prepare_diff()
        .context("preparing baseline-to-worktree target diff")?;
    let hunks = match prepared.operation {
        Operation::InternalDiff { algorithm } => {
            let input = prepared.interned_input();
            let sink = UnifiedDiff::new(
                &input,
                String::new(),
                NewlineSeparator::AfterHeaderAndWhenNeeded("\n"),
                ContextSize::default(),
            );
            gix::diff::blob::diff(algorithm, &input, sink)
                .context("rendering baseline-to-worktree diff hunks")?
        }
        Operation::ExternalCommand { .. } | Operation::SourceOrDestinationIsBinary => {
            "Binary files differ\n".to_string()
        }
    };
    let metadata_changed = !same_path || !same_mode || baseline.is_none();
    if hunks.is_empty() && !metadata_changed {
        return Ok(String::new());
    }

    let old_display = old_path.to_string_lossy();
    let new_display = target_path.to_string_lossy();
    let mut output = format!("diff --gix a/{old_display} b/{new_display}\n");
    match baseline.as_ref() {
        None => output.push_str(&format!("new file mode {new_mode:o}\n")),
        Some(entry) if entry.mode != new_mode => output.push_str(&format!(
            "old mode {old:o}\nnew mode {new_mode:o}\n",
            old = entry.mode
        )),
        _ => {}
    }
    if !same_path {
        output.push_str(&format!(
            "rename from {old_display}\nrename to {new_display}\n"
        ));
    }
    output.push_str(&format!(
        "--- {old}\n+++ b/{new_display}\n",
        old = baseline
            .as_ref()
            .map(|_| format!("a/{old_display}"))
            .unwrap_or_else(|| "/dev/null".to_string()),
    ));
    output.push_str(&hunks);
    Ok(output)
}

fn worktree_blob_mode(metadata: &std::fs::Metadata) -> (u16, gix::object::tree::EntryKind) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            return (0o100755, gix::object::tree::EntryKind::BlobExecutable);
        }
    }
    (0o100644, gix::object::tree::EntryKind::Blob)
}

fn target_paths_through_commit(
    repo: &gix::Repository,
    target_path: PathBuf,
    desired: gix::ObjectId,
) -> Result<BTreeSet<PathBuf>> {
    let head = repo.head_id().context("resolving repository HEAD")?;
    let walk = repo
        .rev_walk([head.detach()])
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .all()
        .context("walking target rename history")?;
    let mut tracked_paths = BTreeSet::from([target_path]);
    for info in walk {
        let info = info.context("reading target rename history")?;
        let commit = info.object().context("loading target rename commit")?;
        let current_tree = commit.tree().context("loading target rename tree")?;
        let mut parent_trees = info
            .parent_ids
            .iter()
            .map(|parent_id| {
                repo.find_commit(*parent_id)
                    .context("loading target rename parent")?
                    .tree()
                    .context("loading target rename parent tree")
            })
            .collect::<Result<Vec<_>>>()?;
        if parent_trees.is_empty() {
            parent_trees.push(repo.empty_tree());
        }
        let mut renamed_sources = Vec::new();
        for parent_tree in parent_trees {
            let mut edge_touched = false;
            for path in &tracked_paths {
                if entry_identity(&current_tree, path)? != entry_identity(&parent_tree, path)? {
                    edge_touched = true;
                    break;
                }
            }
            if !edge_touched {
                continue;
            }
            parent_tree
                .changes()
                .context("initializing target rename diff")?
                .options(|options| {
                    options.track_rewrites(Some(Default::default()));
                })
                .for_each_to_obtain_tree(&current_tree, |change| {
                    if let Change::Rewrite {
                        source_location,
                        location,
                        copy: false,
                        ..
                    } = &change
                    {
                        let destination = gix::path::from_bstr(*location);
                        if tracked_paths.contains(destination.as_ref()) {
                            renamed_sources
                                .push(gix::path::from_bstr(*source_location).into_owned());
                        }
                    }
                    Ok::<_, anyhow::Error>(gix::object::tree::diff::Action::Continue)
                })
                .context("following aggregate target renames")?;
        }
        tracked_paths.extend(renamed_sources);
        if info.id == desired {
            return Ok(tracked_paths);
        }
    }
    bail!("oldest target commit is not reachable from repository HEAD")
}

fn entry_identity(tree: &gix::Tree<'_>, path: &Path) -> Result<Option<(gix::ObjectId, u16)>> {
    Ok(tree
        .lookup_entry_by_path(path)
        .context("looking up target in commit tree")?
        .map(|entry| (entry.id().detach(), entry.mode().value())))
}

fn open_repo_and_target(workspace: &Path, target: &str) -> Result<(gix::Repository, PathBuf)> {
    let target = Path::new(target);
    let absolute_target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        workspace.join(target)
    };
    let absolute_target = absolute_target
        .canonicalize()
        .with_context(|| format!("canonicalizing review target {}", absolute_target.display()))?;
    let repo = gix::discover(workspace).context("discovering repository")?;
    let workdir = repo
        .workdir()
        .context("whole-file review requires a worktree")?;
    let workdir = workdir
        .canonicalize()
        .context("canonicalizing repository worktree")?;
    let relative = absolute_target
        .strip_prefix(&workdir)
        .with_context(|| {
            format!(
                "review target {} is outside repository {}",
                absolute_target.display(),
                workdir.display()
            )
        })?
        .to_path_buf();
    Ok((repo, relative))
}

#[cfg(test)]
mod tests {

    fn risk(name: &str, rating: u8, reason: &str) -> FunctionRisk {
        FunctionRisk {
            name: name.into(),
            risk_rating: rating,
            reason: reason.into(),
        }
    }

    fn partition(
        target: Vec<FunctionRisk>,
        external: Vec<ExternalFunctionRisk>,
    ) -> ChangeSurveyReport {
        ChangeSurveyReport {
            baseline: "base".into(),
            head: "head".into(),
            target_function_risks: target,
            external_major_risks: external,
        }
    }

    /// Each partition sees one source scope, so the whole-file answer
    /// is their union. This replaced a reduction inference call that
    /// invented a function name on kernel/sched/fair.c and then missed
    /// functions on the retry, failing the review bootstrap outright.
    #[test]
    fn merge_unions_partitions_and_keeps_the_highest_rating() {
        let merged = merge_change_survey_reports(
            "base",
            "head",
            vec![
                partition(
                    vec![risk("a", 10, "saw a little"), risk("b", 40, "b changed")],
                    vec![],
                ),
                partition(
                    vec![risk("a", 70, "saw the whole rewrite"), risk("c", 5, "c")],
                    vec![],
                ),
            ],
        );
        let got: Vec<(&str, u8, &str)> = merged
            .target_function_risks
            .iter()
            .map(|r| (r.name.as_str(), r.risk_rating, r.reason.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a", 70, "saw the whole rewrite"),
                ("b", 40, "b changed"),
                ("c", 5, "c"),
            ],
            "a partition that could not see a function must not outvote one that could"
        );
        assert_eq!(merged.baseline, "base");
        assert_eq!(merged.head, "head");
    }

    #[test]
    fn merge_keys_external_risks_by_name_and_file() {
        let ext = |name: &str, file: &str, rating: u8| ExternalFunctionRisk {
            name: name.into(),
            file: file.into(),
            risk_rating: rating,
            reason: "r".into(),
        };
        let merged = merge_change_survey_reports(
            "base",
            "head",
            vec![
                partition(
                    vec![],
                    vec![ext("helper", "mm/a.c", 80), ext("helper", "mm/b.c", 90)],
                ),
                partition(vec![], vec![ext("helper", "mm/a.c", 95)]),
            ],
        );
        // Static helpers share names across translation units, so the
        // identity is (name, file) — collapsing on name alone would
        // silently drop one of them.
        assert_eq!(merged.external_major_risks.len(), 2);
        let a = merged
            .external_major_risks
            .iter()
            .find(|r| r.file == "mm/a.c")
            .unwrap();
        assert_eq!(a.risk_rating, 95);
    }

    /// A function no partition rated is NOT evidence of zero risk, so
    /// the merge must leave it absent rather than manufacturing a
    /// rating. The caller resolves the gap with a corrective pass.
    #[test]
    fn merge_never_manufactures_a_rating_for_an_unrated_function() {
        let merged = merge_change_survey_reports(
            "base",
            "head",
            vec![partition(vec![risk("a", 10, "a")], vec![])],
        );
        assert_eq!(merged.target_function_risks.len(), 1);
        let expected: BTreeSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert!(validate_function_coverage(&merged, &expected).is_err());
        assert_eq!(
            unrated_functions(&merged, &expected),
            ["b", "c"]
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>()
        );
    }

    /// The fair.c shape: partitions under-report badly, so the gap
    /// must be batched by function count rather than demanded in one
    /// call. 51 rated of 521 leaves 470, which at 150 per batch is
    /// four calls — none of them the 470-name roster that failed.
    #[test]
    fn a_large_coverage_gap_is_split_into_bounded_batches() {
        let merged = merge_change_survey_reports(
            "base",
            "head",
            vec![partition(
                (0..51).map(|i| risk(&format!("f{i}"), 5, "seen")).collect(),
                vec![],
            )],
        );
        let expected: BTreeSet<String> = (0..521).map(|i| format!("f{i}")).collect();
        let missing = unrated_functions(&merged, &expected);
        assert_eq!(missing.len(), 470);
        let batch = 150;
        let batches = missing.len().div_ceil(batch);
        assert_eq!(batches, 4);
        // Every missing name lands in exactly one batch: the union of
        // the batches must be the gap, with nothing invented or lost.
        let chunks: Vec<Vec<&String>> = missing
            .iter()
            .collect::<Vec<_>>()
            .chunks(batch)
            .map(<[&String]>::to_vec)
            .collect();
        assert!(chunks.iter().all(|c| c.len() <= batch));
        let rejoined: BTreeSet<String> = chunks.into_iter().flatten().cloned().collect();
        assert_eq!(rejoined, missing);
    }

    /// The fair.c shape end to end. Batches come back SHORT, not
    /// wrong: 150/150, 147/150, 63/63. Validating each all-or-nothing
    /// discarded the 147 and failed the run, so the loop must merge
    /// short answers and re-ask for the remainder.
    #[test]
    fn short_batch_answers_are_kept_and_the_remainder_is_re_asked() {
        let expected: BTreeSet<String> = (0..421).map(|i| format!("f{i}")).collect();
        // Partitions covered 58; 363 unrated, as the run reported.
        let mut merged = merge_change_survey_reports(
            "base",
            "head",
            vec![partition(
                (0..58).map(|i| risk(&format!("f{i}"), 5, "seen")).collect(),
                vec![],
            )],
        );
        let round1 = unrated_functions(&merged, &expected);
        assert_eq!(round1.len(), 363);

        // Round 1: three batches returning 150, 147 and 63.
        let names: Vec<&String> = round1.iter().collect();
        let mut answered: Vec<FunctionRisk> = Vec::new();
        for (start, got) in [(0usize, 150usize), (150, 147), (300, 63)] {
            for name in names.iter().skip(start).take(got) {
                answered.push(risk(name, 20, "rated"));
            }
        }
        merged =
            merge_change_survey_reports("base", "head", vec![merged, partition(answered, vec![])]);
        let round2 = unrated_functions(&merged, &expected);
        assert_eq!(round2.len(), 3, "only the three dropped names remain");
        assert!(round2.len() < round1.len(), "the round made progress");

        // Round 2 closes it.
        merged = merge_change_survey_reports(
            "base",
            "head",
            vec![
                merged,
                partition(
                    round2.iter().map(|n| risk(n, 30, "rated")).collect(),
                    vec![],
                ),
            ],
        );
        assert!(unrated_functions(&merged, &expected).is_empty());
        validate_function_coverage(&merged, &expected).unwrap();
    }

    #[test]
    fn merge_reports_no_gap_when_the_partitions_cover_everything() {
        let merged = merge_change_survey_reports(
            "base",
            "head",
            vec![
                partition(vec![risk("a", 1, "a")], vec![]),
                partition(vec![risk("b", 2, "b")], vec![]),
            ],
        );
        let expected: BTreeSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        assert!(unrated_functions(&merged, &expected).is_empty());
        assert!(validate_function_coverage(&merged, &expected).is_ok());
    }

    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn git_at(dir: &Path, args: &[&str], timestamp: i64) {
        let date = format!("@{timestamp}");
        let status = Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn recent_history_skips_merges_and_unrelated_commits() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        git(tmp.path(), &["config", "user.email", "kres@example.com"]);
        git(tmp.path(), &["config", "user.name", "kres test"]);
        std::fs::write(
            tmp.path().join("target.c"),
            "int target(void) { return 0; }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("other.c"),
            "int other(void) { return 0; }\n",
        )
        .unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-q", "-m", "base"]);
        std::fs::write(
            tmp.path().join("other.c"),
            "int other(void) { return 1; }\n",
        )
        .unwrap();
        git(tmp.path(), &["commit", "-q", "-am", "unrelated"]);
        std::fs::write(
            tmp.path().join("target.c"),
            "int target(void) { return 2; }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("other.c"),
            "int other(void) { return 2; }\n",
        )
        .unwrap();
        git(tmp.path(), &["commit", "-q", "-am", "target and other"]);
        git(tmp.path(), &["checkout", "-q", "-b", "side"]);
        std::fs::write(
            tmp.path().join("target.c"),
            "int target(void) { return 3; }\n",
        )
        .unwrap();
        git(tmp.path(), &["commit", "-q", "-am", "side target"]);
        git(tmp.path(), &["checkout", "-q", "main"]);
        std::fs::write(
            tmp.path().join("other.c"),
            "int other(void) { return 3; }\n",
        )
        .unwrap();
        git(tmp.path(), &["commit", "-q", "-am", "main other"]);
        git(
            tmp.path(),
            &["merge", "-q", "--no-ff", "side", "-m", "merge side"],
        );

        let commits = recent_target_commits(tmp.path(), "target.c", 0).unwrap();
        assert_eq!(commits.len(), 3);
        let merge_id = gix::discover(tmp.path())
            .unwrap()
            .head_id()
            .unwrap()
            .to_string();
        assert!(commits.iter().all(|commit| commit.id != merge_id));

        let aggregate = aggregate_target_diff(tmp.path(), "target.c", 0).unwrap();
        assert!(aggregate.diff.contains("target.c"));
        assert!(!aggregate.diff.contains("other.c"));
    }
    #[test]
    fn aggregate_diff_contains_final_target_state_not_intermediate_commits() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        git(tmp.path(), &["config", "user.email", "kres@example.com"]);
        git(tmp.path(), &["config", "user.name", "kres test"]);
        std::fs::write(
            tmp.path().join("target.c"),
            "int target(void) { return 0; }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("other.c"),
            "int other(void) { return 0; }\n",
        )
        .unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-q", "-m", "base"]);
        std::fs::write(
            tmp.path().join("target.c"),
            "int target(void) { return 1; }\n",
        )
        .unwrap();
        git(tmp.path(), &["commit", "-q", "-am", "intermediate target"]);
        std::fs::write(
            tmp.path().join("target.c"),
            "int target(void) { return 2; }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("other.c"),
            "int other(void) { return 2; }\n",
        )
        .unwrap();
        git(
            tmp.path(),
            &["commit", "-q", "-am", "final target and other"],
        );

        let aggregate = aggregate_target_diff(tmp.path(), "target.c", 0).unwrap();
        assert!(aggregate.diff.contains("return 2"));
        assert!(!aggregate.diff.contains("return 1"));
        assert!(!aggregate.diff.contains("other.c"));
    }

    #[test]
    fn aggregate_diff_ends_at_worktree_not_head() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        git(tmp.path(), &["config", "user.email", "kres@example.com"]);
        git(tmp.path(), &["config", "user.name", "kres test"]);
        std::fs::write(
            tmp.path().join("target.c"),
            "int target(void) { return 0; }\n",
        )
        .unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-q", "-m", "base"]);
        std::fs::write(
            tmp.path().join("target.c"),
            "int target(void) { return 1; }\n",
        )
        .unwrap();
        git(tmp.path(), &["commit", "-q", "-am", "committed change"]);
        std::fs::write(
            tmp.path().join("target.c"),
            "int target(void) { return 2; }\n",
        )
        .unwrap();

        let aggregate = aggregate_target_diff(tmp.path(), "target.c", 0).unwrap();
        assert!(aggregate.head.starts_with("WORKTREE@"));
        assert!(aggregate.diff.contains("return 2"));
        assert!(!aggregate.diff.contains("return 1"));
    }

    #[test]
    fn aggregate_diff_ignores_reused_historical_path() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        git(tmp.path(), &["config", "user.email", "kres@example.com"]);
        git(tmp.path(), &["config", "user.name", "kres test"]);
        std::fs::write(tmp.path().join("old.c"), "int target(void) { return 0; }\n").unwrap();
        git(tmp.path(), &["add", "."]);
        git_at(tmp.path(), &["commit", "-q", "-m", "base"], 1_700_000_000);
        git(tmp.path(), &["mv", "old.c", "target.c"]);
        git_at(
            tmp.path(),
            &["commit", "-q", "-m", "rename target"],
            1_700_000_200,
        );
        std::fs::write(
            tmp.path().join("old.c"),
            "int unrelated(void) { return 99; }\n",
        )
        .unwrap();
        git(tmp.path(), &["add", "old.c"]);
        git_at(
            tmp.path(),
            &["commit", "-q", "-m", "reuse old path"],
            1_700_000_300,
        );

        let aggregate = aggregate_target_diff(tmp.path(), "target.c", 1_700_000_100).unwrap();
        assert!(aggregate.diff.contains("rename from old.c"));
        assert!(aggregate.diff.contains("rename to target.c"));
        assert!(!aggregate.diff.contains("unrelated"));
    }

    #[test]
    fn parser_keeps_only_major_external_risks() {
        let parsed = parse_inference_risks(
            r#"{"target_function_risks":[{"name":"target","risk_rating":60,"reason":"changed"}],"external_major_risks":[{"name":"major","file":"other.c","risk_rating":90,"reason":"lifetime"},{"name":"minor","file":"other.c","risk_rating":70,"reason":"small"}]}"#,
            "base",
            "head",
        )
        .unwrap();
        assert_eq!(parsed.target_function_risks.len(), 1);
        assert_eq!(parsed.external_major_risks.len(), 1);
        assert_eq!(parsed.external_major_risks[0].name, "major");

        assert!(parse_inference_risks(
            r#"{"target_function_risks":[{"name":"target","risk_rating":60,"reason":"first"},{"name":"target","risk_rating":70,"reason":"second"}],"external_major_risks":[]}"#,
            "base",
            "head",
        )
        .is_err());
    }

    #[test]
    fn sparse_report_completion_requires_corrective_inference() {
        let expected = BTreeSet::from(["first".to_string(), "second".to_string()]);
        let sparse = ChangeSurveyReport {
            baseline: "base".into(),
            head: "head".into(),
            target_function_risks: vec![FunctionRisk {
                name: "first".into(),
                risk_rating: 40,
                reason: "first evidence".into(),
            }],
            external_major_risks: Vec::new(),
        };

        assert!(complete_function_coverage(sparse, &expected).is_err());
    }

    #[test]
    fn diff_chunks_preserve_source_bytes_and_repeat_hunk_context() {
        let diff = "diff --gix a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,4 +1,4 @@\n-old one\n+new one\n context one\n context two\n@@ -20,4 +20,4 @@\n-old two\n+new two\n context three\n context four\n";
        let chunks = split_diff_for_inference(diff, 160).unwrap();

        assert_eq!(
            chunks
                .iter()
                .map(|chunk| &diff[chunk.source_start..chunk.source_end])
                .collect::<String>(),
            diff
        );
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.text.len() <= 160));
        assert!(chunks[1].text.starts_with("diff --gix a/a.c b/a.c"));
        assert!(chunks[1].text.contains("CHUNK CONTINUATION"));
        assert!(diff.is_char_boundary(chunks[1].source_start));
        assert_eq!(diff.as_bytes()[chunks[1].source_start - 1], b'\n');
    }

    #[test]
    fn coverage_requires_each_authoritative_function_once() {
        let expected = BTreeSet::from(["first".to_string(), "second".to_string()]);
        let complete = ChangeSurveyReport {
            baseline: "base".into(),
            head: "head".into(),
            target_function_risks: vec![
                FunctionRisk {
                    name: "first".into(),
                    risk_rating: 10,
                    reason: "unchanged".into(),
                },
                FunctionRisk {
                    name: "second".into(),
                    risk_rating: 20,
                    reason: "nearby change".into(),
                },
            ],
            external_major_risks: Vec::new(),
        };
        validate_function_coverage(&complete, &expected).unwrap();

        let mut incomplete = complete.clone();
        incomplete.target_function_risks.pop();
        assert!(validate_function_coverage(&incomplete, &expected).is_err());

        let mut duplicate = complete;
        duplicate
            .target_function_risks
            .push(duplicate.target_function_risks[0].clone());
        assert!(validate_function_coverage(&duplicate, &expected).is_err());
    }

    #[test]
    fn recent_history_follows_target_across_renames() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        git(tmp.path(), &["config", "user.email", "kres@example.com"]);
        git(tmp.path(), &["config", "user.name", "kres test"]);
        std::fs::write(tmp.path().join("old.c"), "int target(void) { return 0; }\n").unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-q", "-m", "base"]);
        std::fs::write(tmp.path().join("old.c"), "int target(void) { return 1; }\n").unwrap();
        git(tmp.path(), &["commit", "-q", "-am", "modify before rename"]);
        git(tmp.path(), &["mv", "old.c", "target.c"]);
        git(tmp.path(), &["commit", "-q", "-m", "rename target"]);

        let commits = recent_target_commits(tmp.path(), "target.c", 0).unwrap();
        assert_eq!(commits.len(), 3, "pre-rename history must be retained");
        let aggregate = aggregate_target_diff(tmp.path(), "target.c", 0).unwrap();
        assert!(aggregate.diff.contains("target.c"));
        assert!(aggregate.diff.contains("return 1"));
    }

    #[test]
    fn aggregate_diff_includes_mode_only_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        git(tmp.path(), &["config", "user.email", "kres@example.com"]);
        git(tmp.path(), &["config", "user.name", "kres test"]);
        std::fs::write(tmp.path().join("target.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        git(tmp.path(), &["add", "."]);
        git_at(tmp.path(), &["commit", "-q", "-m", "base"], 1_700_000_000);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(tmp.path().join("target.sh"))
                .unwrap()
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(tmp.path().join("target.sh"), permissions).unwrap();
        }
        git(tmp.path(), &["add", "target.sh"]);
        git_at(
            tmp.path(),
            &["commit", "-q", "-m", "make executable"],
            1_700_000_200,
        );

        let aggregate = aggregate_target_diff(tmp.path(), "target.sh", 1_700_000_100).unwrap();
        assert!(aggregate.diff.contains("old mode 100644"));
        assert!(aggregate.diff.contains("new mode 100755"));
    }

    #[test]
    fn prompt_requires_low_ratings_instead_of_empty_target_ratings() {
        let window = AggregateTargetDiff {
            baseline: "base".into(),
            head: "head".into(),
            diff: "diff".into(),
        };
        let prompt = change_survey_prompt("target.c", "int f(void);", &window, None);
        assert!(prompt
            .cached_prefix
            .contains("still needs a low, evidence-based rating"));
        assert!(!prompt.cached_prefix.contains("Return empty arrays"));
        assert!(prompt.cached_prefix.contains("int f(void);"));
        assert!(!prompt.cached_prefix.contains("BASELINE: base"));
        assert!(prompt.tail.contains("BASELINE: base"));
        assert!(prompt.tail.contains("SIX-MONTH TARGET-FILE DIFF:\ndiff"));
    }
}
