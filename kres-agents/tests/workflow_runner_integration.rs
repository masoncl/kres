//! End-to-end workflow-runner test: spins up a tiny in-process
//! HTTP server that replies with a canned Anthropic JSON body, then
//! drives the FIX workflow through `LlmDriver::run` and asserts the
//! executor reaches `Success` after consuming the responses.
//!
//! Why integration-style instead of a unit test: `LlmDriver::run`
//! talks to a `kres_llm::client::Client` over reqwest. There's no
//! trait abstraction to swap in, so the only realistic way to
//! cover the call path is to point the client at a real HTTP
//! endpoint. A 60-line `tokio::net::TcpListener` mock keeps deps
//! minimal — no wiremock / mockito.

use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use kres_agents::workflow::parse_workflow;
use kres_agents::workflow_exec::{run, WorkflowStatus};
use kres_agents::workflow_runner::{derive_inputs, AgentEnv, LlmDriver};
use kres_llm::client::Client;

/// Build a fake `messages` JSON envelope with `text` as the single
/// content block. Mirrors the subset of the Anthropic response
/// shape the kres-llm client deserialises.
fn fake_messages_response(text: &str) -> Value {
    json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 10,
            "output_tokens": 20,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        },
        "content": [{"type": "text", "text": text}]
    })
}

/// Mock orchestrator response picking the given next step. The
/// orchestrator runs on review.clean==false and routes via its
/// next_step output.
fn orchestrator_picks(next_step: &str) -> Value {
    fake_messages_response(&format!(
        "Orchestrator pick.\n{{\"next_step\": \"{next_step}\", \"instruction\": \"\", \"rationale\": \"test fixture\"}}"
    ))
}

fn review_ledger_response(kind: &str, status: &str) -> Value {
    fake_messages_response(&format!(
        "{{\"ledger\": [{{\"id\": \"R1\", \"kind\": \"{kind}\", \"status\": \"{status}\", \
         \"summary\": \"test review complaint\", \"latest\": \"test fixture\", \
         \"history\": [{{\"step\": \"test\", \"attempt\": 1, \"action\": \"mapped\", \
         \"note\": \"fixture ledger update\"}}]}}]}}"
    ))
}

struct ResearchFixture<'a> {
    status: &'a str,
    valid: bool,
    invalid_evidence: &'a str,
    invalid_evidence_kind: &'a str,
    affected_files: &'a [&'a str],
    affected_symbols: &'a [&'a str],
    research_decision: Value,
    analysis: &'a str,
}

fn research_response(lead: &str, fixture: ResearchFixture<'_>) -> Value {
    research_response_with_latent(lead, fixture, false)
}

fn research_response_with_latent(
    lead: &str,
    fixture: ResearchFixture<'_>,
    is_latent: bool,
) -> Value {
    let body = json!({
        "research_status": fixture.status,
        "valid": fixture.valid,
        "invalid_evidence": fixture.invalid_evidence,
        "invalid_evidence_kind": fixture.invalid_evidence_kind,
        "affected_files": fixture.affected_files,
        "affected_symbols": fixture.affected_symbols,
        "fix_plan": [{
            "id": "fix-1",
            "title": "Fix test bug",
            "scope": "test fixture scope",
            "affected_files": fixture.affected_files,
            "affected_symbols": fixture.affected_symbols,
            "fix_contract": fixture.analysis,
            "rationale": "single-commit fixture",
            "depends_on": []
        }],
        "research_decision": fixture.research_decision,
        "is_latent": is_latent,
        "analysis": fixture.analysis,
    });
    fake_messages_response(&format!(
        "{lead}\n{}",
        serde_json::to_string(&body).unwrap()
    ))
}

fn confirmed_research_response(
    lead: &str,
    affected_files: &[&str],
    affected_symbols: &[&str],
    analysis: &str,
) -> Value {
    research_response(
        lead,
        ResearchFixture {
            status: "confirmed",
            valid: true,
            invalid_evidence: "",
            invalid_evidence_kind: "none",
            affected_files,
            affected_symbols,
            research_decision: json!({
                "bug_proven": true,
                "fix_contract_proven": true,
                "invalidity_proven": false,
                "needs_more_audit": false,
            }),
            analysis,
        },
    )
}

/// Canonical empty-result for the lore-search step. The fix workflow
/// runs this between research and write-patch; tests that don't care
/// about lore findings still need an entry in the mock queue so
/// write-patch sees its own response.
fn empty_lore_search_response() -> Value {
    fake_messages_response(
        "No upstream patch found.\n\
         {\"existing_patches\": [], \
          \"duplicate_proven\": false, \
          \"analysis\": \"Lore search returned no matching threads.\"}",
    )
}

fn invalid_research_response(lead: &str, invalid_evidence: &str, analysis: &str) -> Value {
    research_response(
        lead,
        ResearchFixture {
            status: "invalid",
            valid: false,
            invalid_evidence,
            invalid_evidence_kind: "source_or_commit_evidence",
            affected_files: &[],
            affected_symbols: &[],
            research_decision: json!({
                "bug_proven": false,
                "fix_contract_proven": false,
                "invalidity_proven": true,
                "needs_more_audit": false,
            }),
            analysis,
        },
    )
}

fn clean_review_responses(workflow: &kres_agents::workflow::Workflow) -> Vec<Value> {
    let review = workflow
        .steps
        .iter()
        .find(|s| s.id == "review")
        .expect("fix workflow has review step");
    // One response for each lens plus one consolidate response.
    (0..=review.lenses.len())
        .map(|_| {
            fake_messages_response(
                "Review clean.\n\
                 {\"clean\": true, \"defects\": [], \"analysis\": \"review clean\", \
                  \"correction_step\": \"write-patch\"}",
            )
        })
        .collect()
}

fn dirty_source_review_responses(workflow: &kres_agents::workflow::Workflow) -> Vec<Value> {
    let review = workflow
        .steps
        .iter()
        .find(|s| s.id == "review")
        .expect("fix workflow has review step");
    let mut responses = Vec::new();
    responses.push(fake_messages_response(
        "Source defect found.\n\
         {\"clean\": false, \
          \"defects\": [{\"where\": \"a.c\", \"what\": \"use the reviewed correction\"}], \
          \"source_defects\": [{\"where\": \"a.c\", \"what\": \"use the reviewed correction\"}], \
          \"commit_message_defects\": [], \
          \"analysis\": \"a.c still has the wrong value\", \
          \"correction_step\": \"write-patch\"}",
    ));
    for _ in 1..review.lenses.len() {
        responses.push(fake_messages_response(
            "Review clean.\n\
             {\"clean\": true, \"defects\": [], \"source_defects\": [], \
              \"commit_message_defects\": [], \"analysis\": \"review clean\", \
              \"correction_step\": \"write-patch\"}",
        ));
    }
    // Consolidate LLM call (semantic dedup of typed lists; routing
    // fields are overridden deterministically afterwards).
    responses.push(fake_messages_response(
        "Consolidate dirty source review.\n\
         {\"clean\": false, \
          \"defects\": [{\"where\": \"a.c\", \"what\": \"use the reviewed correction\", \"lens\": \"lifetime\"}], \
          \"source_defects\": [{\"where\": \"a.c\", \"what\": \"use the reviewed correction\", \"lens\": \"lifetime\"}], \
          \"commit_message_defects\": [], \
          \"unresolved_risks\": [], \
          \"outcomes\": [], \
          \"analysis\": \"consolidated\", \
          \"correction_step\": \"write-patch\"}",
    ));
    responses
}

fn dirty_commit_message_review_responses(workflow: &kres_agents::workflow::Workflow) -> Vec<Value> {
    let review = workflow
        .steps
        .iter()
        .find(|s| s.id == "review")
        .expect("fix workflow has review step");
    let mut responses = Vec::new();
    responses.push(fake_messages_response(
        "Commit message defect found.\n\
         {\"clean\": false, \
          \"defects\": [{\"where\": \"commit message\", \"what\": \"rewrite the stale claim\"}], \
          \"source_defects\": [], \
          \"commit_message_defects\": [{\"where\": \"commit message\", \"what\": \"rewrite the stale claim\"}], \
          \"analysis\": \"the message overstates the fix\", \
          \"correction_step\": \"write-commit-message\"}",
    ));
    for _ in 1..review.lenses.len() {
        responses.push(fake_messages_response(
            "Review clean.\n\
             {\"clean\": true, \"defects\": [], \"source_defects\": [], \
              \"commit_message_defects\": [], \"analysis\": \"review clean\", \
              \"correction_step\": \"write-patch\"}",
        ));
    }
    responses.push(fake_messages_response(
        "Consolidate dirty commit-message review.\n\
         {\"clean\": false, \
          \"defects\": [{\"where\": \"commit message\", \"what\": \"rewrite the stale claim\", \"lens\": \"maintainer\"}], \
          \"source_defects\": [], \
          \"commit_message_defects\": [{\"where\": \"commit message\", \"what\": \"rewrite the stale claim\", \"lens\": \"maintainer\"}], \
          \"unresolved_risks\": [], \
          \"outcomes\": [], \
          \"analysis\": \"consolidated\", \
          \"correction_step\": \"write-commit-message\"}",
    ));
    responses
}

/// Run a thread that accepts one connection per response in
/// `responses`, reads the request, records it, and writes a
/// single-shot HTTP/1.1 reply containing the canned JSON body.
/// Returns the bound port. Spawns inline; the listener stays alive
/// until the responses queue is drained.
async fn spawn_recording_mock(responses: VecDeque<Value>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let queue = Arc::new(Mutex::new(responses));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            // Drain one request chunk. The test prompts are small enough
            // for this mock, and reqwest is happy once the server answers.
            let mut buf = vec![0u8; 65536];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            recorded
                .lock()
                .await
                .push(String::from_utf8_lossy(&buf[..n]).into_owned());
            let body = {
                let mut q = queue.lock().await;
                match q.pop_front() {
                    Some(v) => v.to_string(),
                    None => json!({"error": "no more canned responses"}).to_string(),
                }
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (port, requests)
}

async fn spawn_mock(responses: VecDeque<Value>) -> u16 {
    let (port, _) = spawn_recording_mock(responses).await;
    port
}

/// Helper: build a slow `AgentEnv` pointed at the mock server.
fn slow_env_pointing_at(port: u16) -> AgentEnv {
    let client = Client::builder("test-key")
        .base_url(format!("http://127.0.0.1:{port}"))
        .no_proxy() // bypass HTTPS_PROXY in CI sandboxes
        .build()
        .unwrap();
    AgentEnv::new(
        Arc::new(client),
        "claude-haiku-4-5-20251001",
        4_096,
        Some("test system prompt".to_string()),
    )
}

/// Helper: build a fast `AgentEnv` pointed at the mock server.
fn fast_env_pointing_at(port: u16) -> AgentEnv {
    slow_env_pointing_at(port)
}

/// Set up a tempdir as a fresh git repo with one tracked file so
/// the FIX workflow's post_actions (`git add` / `git commit`) can
/// land. Returns the tempdir guard (drop = cleanup) and the
/// workspace path.
fn fresh_git_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().to_path_buf();
    let runs = [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
        vec!["config", "commit.gpgsign", "false"],
    ];
    for args in runs {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    }
    std::fs::write(workspace.join("a.c"), "int x = 1;\n").unwrap();
    std::fs::write(workspace.join("b.h"), "#define B 1\n").unwrap();
    let out = std::process::Command::new("git")
        .args(["add", "a.c", "b.h"])
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(out.status.success());
    let out = std::process::Command::new("git")
        .args(["commit", "-q", "-m", "initial"])
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(out.status.success(), "initial commit failed: {:?}", out);
    (tmp, workspace)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix_workflow_runs_end_to_end_against_mock_llm() {
    // Canned LLM responses, in the order steps will fire:
    //   1. research: valid=true + affected files
    //   2. write-patch: build_target + code_edits
    //   3. fixes-tag-search: proven fixes_sha
    //   4. write-commit-message: commit-message code_output
    //   5..N. review lenses: clean=true
    //   N+1. review consolidate: clean=true
    //
    // The model emits prose + a trailing JSON object — the runner's
    // extract_outputs picks the JSON. Publish runs as a reaper step
    // (no LLM call) — we point it at a non-finding-dir target_kind
    // so it's skipped via run_if. Commit and build are deterministic
    // reaper steps. Status updates are skipped because research is confirmed.
    let workflow = parse_workflow(include_str!("../../configs/workflows/fix.json")).unwrap();
    let mut responses = VecDeque::from(vec![
        confirmed_research_response(
            "I traced the bug. Reasoning here.",
            &["a.c"],
            &["f"],
            "I traced the bug. Reasoning here.",
        ),
        empty_lore_search_response(),
        fake_messages_response(
            "Wrote the fix.\n\
             {\"build_target\": \"a.o\", \
              \"code_edits\": [{\"file_path\": \"a.c\", \
              \"old_string\": \"int x = 1;\\n\", \
              \"new_string\": \"int x = 2;\\n\"}]}",
        ),
        fake_messages_response(
            "Found the introducing commit.\n\
             {\"fixes_sha\": \"abc123def456\", \
              \"fixes_subject\": \"subsystem: original buggy commit\", \
              \"fixes_evidence\": \"The preimage lacks the bug and the postimage has it.\", \
              \"unproven_fixes_candidates\": [], \
              \"analysis\": \"Checked blame, --follow, and pickaxe.\"}",
        ),
        fake_messages_response(
            "Wrote the commit message.\n\
             {\"code_output\": [{\"path\": \".kres-commit-msg.tmp\", \
              \"content\": \"subsystem: fix the bug\\n\\nBody explaining the fix.\\n\\nAssisted-by: kres:test\\n\", \
              \"purpose\": \"commit message\"}]}",
        ),
    ]);
    responses.extend(clean_review_responses(&workflow));
    let (port, requests) = spawn_recording_mock(responses).await;

    // target_kind=prose so:
    //   - invalidate is skipped (run_if requires finding_dir)
    //   - publish is skipped (run_if requires finding_dir)
    //   - the three LLM steps plus deterministic commit/build run in sequence
    let mut inputs = Map::new();
    inputs.insert("target".into(), Value::String("freeform bug prose".into()));
    inputs.insert("assisted_by".into(), Value::String("kres:test".into()));
    let inputs = derive_inputs(&workflow, inputs);
    assert_eq!(inputs.get("target_kind"), Some(&json!("prose")));

    let (_guard, workspace) = fresh_git_repo();
    let mut driver = LlmDriver::new(workspace, workflow.clone())
        .with_fast(fast_env_pointing_at(port))
        .with_slow(slow_env_pointing_at(port))
        .with_code(slow_env_pointing_at(port));

    let trace = run(&workflow, &mut driver, inputs).await;
    eprintln!("{}", trace.pretty());
    // fix.json's completion.success_when_any matches once review.clean
    // is true under target_kind=prose, so the workflow lands as
    // TerminalSuccess. Either Success or TerminalSuccess is fine for
    // the wiring this test exercises.
    assert!(
        matches!(
            trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ),
        "expected Success or TerminalSuccess, got {:?}",
        trace.status
    );

    // Each LLM step ran exactly once and produced its declared keys.
    let produced =
        |id: &str| -> Map<String, Value> {
            trace
                .events
                .iter()
                .find_map(|e| match e {
                    kres_agents::workflow_exec::TraceEvent::StepProduced {
                        id: i, outputs, ..
                    } if i == id => Some(outputs.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
    let r = produced("research");
    assert_eq!(r.get("valid"), Some(&json!(true)));
    assert_eq!(r.get("research_status"), Some(&json!("confirmed")));
    let requests = requests.lock().await.join("\n---REQUEST---\n");
    assert!(
        requests.contains(
            "Proving a concrete bug requires proving the alleged state is valid and reachable"
        ),
        "research prompt did not require valid/reachable state proof"
    );
    assert!(
        requests.contains("restores behavior that current history deliberately removed"),
        "research prompt did not require checking deliberately removed behavior"
    );
    assert!(
        requests.contains("creator path -> transformed state"),
        "research prompt did not require a concrete trigger path"
    );

    let wp = produced("write-patch");
    assert_eq!(wp.get("build_target"), Some(&json!("a.o")));
    assert_eq!(wp.get("code_changes_emitted"), Some(&json!(true)));
    assert_eq!(wp.get("affected_files_changed"), Some(&json!(true)));
    assert!(
        !wp.contains_key("commit_sha"),
        "write-patch must not require or carry a commit SHA"
    );

    let fixes = produced("fixes-tag-search");
    assert_eq!(fixes.get("fixes_sha"), Some(&json!("abc123def456")));
    assert_eq!(
        fixes.get("fixes_subject"),
        Some(&json!("subsystem: original buggy commit"))
    );

    let msg = produced("write-commit-message");
    assert_eq!(msg.get("commit_message_written"), Some(&json!(true)));

    let commit = produced("commit");
    assert!(commit.get("commit_sha").and_then(|v| v.as_str()).is_some());

    let build = produced("build");
    assert_eq!(build.get("result"), Some(&json!("clean")));

    let rv = produced("review");
    assert_eq!(rv.get("clean"), Some(&json!(true)));

    // invalidate is skipped via run_if (target_kind=prose).
    // publish never runs — the completion.success_when_any
    // expression matches once review.clean == true and short-
    // circuits the run before publish would be scheduled.
    let invalidate_skipped = trace.events.iter().any(|e| {
        matches!(
            e,
            kres_agents::workflow_exec::TraceEvent::StepSkipped { id, .. } if id == "invalidate"
        )
    });
    assert!(
        invalidate_skipped,
        "invalidate should have been skipped via run_if"
    );
    for forbidden in ["invalidate", "publish"] {
        let was_run = trace.events.iter().any(|e| {
            matches!(
                e,
                kres_agents::workflow_exec::TraceEvent::StepProduced { id, .. } if id == forbidden
            )
        });
        assert!(!was_run, "{forbidden} should not have run");
    }
}

/// When lore-search reports an existing upstream patch, the
/// duplicate_proven + existing_patches outputs must reach the
/// write-patch step's prompt so the patch author can cite the
/// upstream Message-ID. Also confirm the bug-coverage lens prompt
/// sees the same data.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lore_search_findings_thread_through_write_patch_prompt() {
    let workflow = parse_workflow(include_str!("../../configs/workflows/fix.json")).unwrap();
    // Distinctive msgid + URL we can grep for in the recorded
    // write-patch request body to prove the interpolation worked.
    let lore_response = fake_messages_response(
        "Found an upstream patch.\n\
         {\"existing_patches\": [\
            {\"msgid\": \"<20260514.testlore-canary@example.org>\", \
             \"subject\": \"a: fix the same bug upstream\", \
             \"url\": \"https://lore.kernel.org/r/20260514.testlore-canary@example.org\", \
             \"relevance\": \"same site, same semantic change\"}], \
          \"duplicate_proven\": true, \
          \"analysis\": \"Body search for `f` returned a thread proposing the same edit.\"}",
    );
    let mut responses = VecDeque::from(vec![
        confirmed_research_response(
            "Bug is real, but we'll see if upstream beat us to it.",
            &["a.c"],
            &["f"],
            "Patch is the cpu_possible guard.",
        ),
        lore_response,
        fake_messages_response(
            "Wrote the fix.\n\
             {\"build_target\": \"a.o\", \
              \"code_edits\": [{\"file_path\": \"a.c\", \
              \"old_string\": \"int x = 1;\\n\", \
              \"new_string\": \"int x = 2;\\n\"}]}",
        ),
        fake_messages_response(
            "No introducing commit found.\n\
             {\"fixes_sha\": \"\", \"fixes_subject\": \"\", \
              \"fixes_evidence\": \"\", \"unproven_fixes_candidates\": [], \
              \"analysis\": \"empty\"}",
        ),
        fake_messages_response(
            "Wrote the commit message.\n\
             {\"code_output\": [{\"path\": \".kres-commit-msg.tmp\", \
              \"content\": \"a: fix the bug\\n\\nBody.\\n\\nAssisted-by: kres:test\\n\", \
              \"purpose\": \"commit message\"}]}",
        ),
    ]);
    responses.extend(clean_review_responses(&workflow));
    let (port, requests) = spawn_recording_mock(responses).await;

    let mut inputs = Map::new();
    inputs.insert("target".into(), Value::String("freeform bug prose".into()));
    inputs.insert("assisted_by".into(), Value::String("kres:test".into()));
    let inputs = derive_inputs(&workflow, inputs);

    let (_guard, workspace) = fresh_git_repo();
    let mut driver = LlmDriver::new(workspace, workflow.clone())
        .with_fast(fast_env_pointing_at(port))
        .with_slow(slow_env_pointing_at(port))
        .with_code(slow_env_pointing_at(port));

    let trace = run(&workflow, &mut driver, inputs).await;
    eprintln!("{}", trace.pretty());
    assert!(
        matches!(
            trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ),
        "expected Success or TerminalSuccess, got {:?}",
        trace.status
    );

    // lore-search produced typed outputs.
    let lore = trace
        .events
        .iter()
        .find_map(|e| match e {
            kres_agents::workflow_exec::TraceEvent::StepProduced { id, outputs, .. }
                if id == "lore-search" =>
            {
                Some(outputs.clone())
            }
            _ => None,
        })
        .expect("lore-search must produce outputs");
    assert_eq!(lore.get("duplicate_proven"), Some(&json!(true)));
    let patches = lore
        .get("existing_patches")
        .and_then(Value::as_array)
        .expect("existing_patches must be an array");
    assert_eq!(patches.len(), 1);
    assert_eq!(
        patches[0].get("msgid"),
        Some(&json!("<20260514.testlore-canary@example.org>"))
    );

    // The write-patch request must have seen the lore findings in
    // its prompt body. Find the request whose URL is /v1/messages
    // and whose body contains the write-patch step marker plus the
    // lore-canary string.
    let all_requests = requests.lock().await.clone();
    let write_patch_request_carried_lore = all_requests
        .iter()
        .any(|r| r.contains("Step 2: WRITE PATCH ONLY") && r.contains("testlore-canary"));
    assert!(
        write_patch_request_carried_lore,
        "write-patch request body must include the lore-search msgid"
    );

    // The lens-fanout review calls also see the lore findings via
    // the review prompt's LORE-SEARCH section (one substitution
    // pass, shared across every lens), so the bug-coverage lens
    // can classify a bug as `duplicate`. Spot-check by confirming
    // at least one request whose body contains both the
    // bug-coverage lens marker AND the lore canary.
    let lens_saw_lore = all_requests
        .iter()
        .any(|r| r.contains("bug-coverage") && r.contains("testlore-canary"));
    assert!(
        lens_saw_lore,
        "bug-coverage lens request must include the lore-search msgid"
    );
}

/// When the model's first write-patch response has an old_string
/// that doesn't match the file, the runner must re-prompt the model
/// with the apply error instead of failing the step. The second
/// response — with a correct old_string — is then applied and the
/// workflow completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_patch_retries_when_code_edits_apply_fails() {
    let workflow = parse_workflow(include_str!("../../configs/workflows/fix.json")).unwrap();
    let mut responses = VecDeque::from(vec![
        confirmed_research_response("Bug is real.", &["a.c"], &["f"], "Patch a.c."),
        empty_lore_search_response(),
        // First write-patch reply: old_string does not exist in a.c.
        fake_messages_response(
            "First attempt — wrong old_string.\n\
             {\"build_target\": \"a.o\", \
              \"code_edits\": [{\"file_path\": \"a.c\", \
              \"old_string\": \"int y = 99;\\n\", \
              \"new_string\": \"int x = 2;\\n\"}]}",
        ),
        // Second write-patch reply: correct old_string.
        fake_messages_response(
            "Second attempt — corrected.\n\
             {\"build_target\": \"a.o\", \
              \"code_edits\": [{\"file_path\": \"a.c\", \
              \"old_string\": \"int x = 1;\\n\", \
              \"new_string\": \"int x = 2;\\n\"}]}",
        ),
        fake_messages_response(
            "No introducing commit.\n\
             {\"fixes_sha\": \"\", \"fixes_subject\": \"\", \
              \"fixes_evidence\": \"\", \"unproven_fixes_candidates\": [], \
              \"analysis\": \"empty\"}",
        ),
        fake_messages_response(
            "Wrote the commit message.\n\
             {\"code_output\": [{\"path\": \".kres-commit-msg.tmp\", \
              \"content\": \"a: fix the bug\\n\\nBody.\\n\\nAssisted-by: kres:test\\n\", \
              \"purpose\": \"commit message\"}]}",
        ),
    ]);
    responses.extend(clean_review_responses(&workflow));
    let (port, requests) = spawn_recording_mock(responses).await;

    let mut inputs = Map::new();
    inputs.insert("target".into(), Value::String("freeform bug prose".into()));
    inputs.insert("assisted_by".into(), Value::String("kres:test".into()));
    let inputs = derive_inputs(&workflow, inputs);

    let (_guard, workspace) = fresh_git_repo();
    let mut driver = LlmDriver::new(workspace.clone(), workflow.clone())
        .with_fast(fast_env_pointing_at(port))
        .with_slow(slow_env_pointing_at(port))
        .with_code(slow_env_pointing_at(port));

    let trace = run(&workflow, &mut driver, inputs).await;
    assert!(
        matches!(
            trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ),
        "expected Success or TerminalSuccess (the retry should succeed), got {:?}",
        trace.status
    );

    // The retry prompt for the second write-patch attempt MUST contain
    // the apply error from the first attempt so the model knows why
    // its old_string was rejected.
    let all_requests = requests.lock().await.clone();
    let retry_with_apply_err = all_requests.iter().any(|r| {
        r.contains("Step 2: WRITE PATCH ONLY")
            && r.contains("code_edits failed to apply")
            && r.contains("old_string not found")
    });
    assert!(
        retry_with_apply_err,
        "second write-patch request must include the apply error from the first attempt"
    );

    // And the file on disk reflects the SECOND (correct) edit.
    let body = std::fs::read_to_string(workspace.join("a.c")).unwrap();
    assert_eq!(body, "int x = 2;\n");
}

/// When research reports is_latent=true, the write-commit-message
/// prompt must carry both the literal latent line and the interpolated
/// is_latent flag so the agent renders the notice as the first body
/// paragraph (after the subject and the blank line). The notice MUST
/// NOT land on line 1: git treats line 1 as the commit subject and
/// `git log --oneline`/`git format-patch` would surface the notice
/// instead of the real subject.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latent_research_threads_into_commit_message_prompt() {
    let workflow = parse_workflow(include_str!("../../configs/workflows/fix.json")).unwrap();
    let latent_research = research_response_with_latent(
        "Bug is real but no caller reaches it today.",
        ResearchFixture {
            status: "confirmed",
            valid: true,
            invalid_evidence: "",
            invalid_evidence_kind: "none",
            affected_files: &["a.c"],
            affected_symbols: &["f"],
            research_decision: json!({
                "bug_proven": true,
                "fix_contract_proven": true,
                "invalidity_proven": false,
                "needs_more_audit": false,
            }),
            analysis: "Contract violation in dead branch; latent.",
        },
        true,
    );
    let mut responses = VecDeque::from(vec![
        latent_research,
        empty_lore_search_response(),
        fake_messages_response(
            "Wrote the fix.\n\
             {\"build_target\": \"a.o\", \
              \"code_edits\": [{\"file_path\": \"a.c\", \
              \"old_string\": \"int x = 1;\\n\", \
              \"new_string\": \"int x = 2;\\n\"}]}",
        ),
        fake_messages_response(
            "No introducing commit found.\n\
             {\"fixes_sha\": \"\", \"fixes_subject\": \"\", \
              \"fixes_evidence\": \"\", \"unproven_fixes_candidates\": [], \
              \"analysis\": \"empty\"}",
        ),
        fake_messages_response(
            "Wrote the commit message.\n\
             {\"code_output\": [{\"path\": \".kres-commit-msg.tmp\", \
              \"content\": \"a: fix the bug\\n\\nNote: This fixes a latent bug with no known triggers in the kernel today.\\n\\nBody.\\n\\nAssisted-by: kres:test\\n\", \
              \"purpose\": \"commit message\"}]}",
        ),
    ]);
    responses.extend(clean_review_responses(&workflow));
    let (port, requests) = spawn_recording_mock(responses).await;

    let mut inputs = Map::new();
    inputs.insert("target".into(), Value::String("freeform bug prose".into()));
    inputs.insert("assisted_by".into(), Value::String("kres:test".into()));
    let inputs = derive_inputs(&workflow, inputs);

    let (_guard, workspace) = fresh_git_repo();
    let mut driver = LlmDriver::new(workspace, workflow.clone())
        .with_fast(fast_env_pointing_at(port))
        .with_slow(slow_env_pointing_at(port))
        .with_code(slow_env_pointing_at(port));

    let trace = run(&workflow, &mut driver, inputs).await;
    assert!(
        matches!(
            trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ),
        "expected Success or TerminalSuccess, got {:?}",
        trace.status
    );

    // research must have produced is_latent=true.
    let research = trace
        .events
        .iter()
        .find_map(|e| match e {
            kres_agents::workflow_exec::TraceEvent::StepProduced { id, outputs, .. }
                if id == "research" =>
            {
                Some(outputs.clone())
            }
            _ => None,
        })
        .expect("research must produce outputs");
    assert_eq!(research.get("is_latent"), Some(&json!(true)));

    // The write-commit-message request body must carry the latent flag
    // and the literal notice line so the agent emits it verbatim.
    let all_requests = requests.lock().await.clone();
    let commit_msg_body = all_requests
        .iter()
        .find(|r| r.contains("Step 4: WRITE COMMIT MESSAGE ONLY"))
        .expect("write-commit-message request must be recorded");
    assert!(
        commit_msg_body.contains("research.is_latent for this run: true"),
        "commit-message prompt must show is_latent=true"
    );
    assert!(
        commit_msg_body
            .contains("Note: This fixes a latent bug with no known triggers in the kernel today."),
        "commit-message prompt must include the verbatim latent notice"
    );
    // The prompt must explicitly forbid putting the notice on line 1 —
    // git treats line 1 as the commit subject, and any wording that
    // lets the model write the notice as the subject reintroduces the
    // ndisc_redirect_rcv-style slip-through where `git log --oneline`
    // shows the notice instead of the real fix subject.
    assert!(
        commit_msg_body.contains("do not put the notice on") && commit_msg_body.contains("line 1"),
        "commit-message prompt must explicitly forbid putting the latent notice on line 1"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix_workflow_commits_write_patch_files_when_research_file_list_is_empty() {
    let workflow = parse_workflow(include_str!("../../configs/workflows/fix.json")).unwrap();
    let mut responses = VecDeque::from(vec![
        confirmed_research_response(
            "Read access was blocked earlier, but the patch target is known.",
            &[],
            &[],
            "Patch a.c even though research did not return affected_files.",
        ),
        empty_lore_search_response(),
        fake_messages_response(
            "Wrote the fix.\n\
             {\"build_target\": \"a.o\", \
              \"code_edits\": [{\"file_path\": \"a.c\", \
              \"old_string\": \"int x = 1;\\n\", \
              \"new_string\": \"int x = 2;\\n\"}]}",
        ),
        fake_messages_response(
            "No proven introducing commit.\n\
             {\"fixes_sha\": \"\", \
              \"fixes_subject\": \"\", \
              \"fixes_evidence\": \"\", \
              \"unproven_fixes_candidates\": [\"abc123def456 (\\\"initial\\\") - toy repo history is insufficient\"], \
              \"analysis\": \"Checked the available toy history; no real kernel provenance exists.\"}",
        ),
        fake_messages_response(
            "Wrote the commit message.\n\
             {\"code_output\": [{\"path\": \".kres-commit-msg.tmp\", \
              \"content\": \"subsystem: fix empty research files\\n\\nBody explaining the fix.\\n\\nAssisted-by: kres:test\\n\", \
              \"purpose\": \"commit message\"}]}",
        ),
    ]);
    responses.extend(clean_review_responses(&workflow));
    let port = spawn_mock(responses).await;
    let mut inputs = Map::new();
    inputs.insert("target".into(), Value::String("freeform bug prose".into()));
    inputs.insert("assisted_by".into(), Value::String("kres:test".into()));
    let inputs = derive_inputs(&workflow, inputs);

    let (_guard, workspace) = fresh_git_repo();
    let mut driver = LlmDriver::new(workspace.clone(), workflow.clone())
        .with_fast(fast_env_pointing_at(port))
        .with_slow(slow_env_pointing_at(port))
        .with_code(slow_env_pointing_at(port));

    let trace = run(&workflow, &mut driver, inputs).await;
    eprintln!("{}", trace.pretty());
    assert!(
        matches!(
            trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ),
        "expected workflow success, got {:?}",
        trace.status
    );

    let produced =
        |id: &str| -> Map<String, Value> {
            trace
                .events
                .iter()
                .find_map(|e| match e {
                    kres_agents::workflow_exec::TraceEvent::StepProduced {
                        id: i, outputs, ..
                    } if i == id => Some(outputs.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };

    let wp = produced("write-patch");
    assert_eq!(wp.get("changed_files"), Some(&json!(["a.c"])));
    assert_eq!(wp.get("affected_files_changed"), Some(&json!(true)));

    let commit = produced("commit");
    assert!(commit.get("commit_sha").and_then(|v| v.as_str()).is_some());

    let out = std::process::Command::new("git")
        .args(["show", "HEAD:a.c"])
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "int x = 2;\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix_workflow_accepts_review_added_patch_file_outside_research_list() {
    let workflow = parse_workflow(include_str!("../../configs/workflows/fix.json")).unwrap();
    let mut responses = VecDeque::from(vec![
        confirmed_research_response(
            "Research found the C file, but review later requires a header doc update.",
            &["a.c"],
            &["f"],
            "Patch requires a related header contract update.",
        ),
        empty_lore_search_response(),
        fake_messages_response(
            "Updated the related header.\n\
             {\"build_target\": \"a.o\", \
              \"code_edits\": [{\"file_path\": \"b.h\", \
              \"old_string\": \"#define B 1\\n\", \
              \"new_string\": \"#define B 2\\n\"}]}",
        ),
        fake_messages_response(
            "No proven introducing commit.\n\
             {\"fixes_sha\": \"\", \
              \"fixes_subject\": \"\", \
              \"fixes_evidence\": \"\", \
              \"unproven_fixes_candidates\": [], \
              \"analysis\": \"No relevant provenance in toy repo.\"}",
        ),
        fake_messages_response(
            "Wrote the commit message.\n\
             {\"code_output\": [{\"path\": \".kres-commit-msg.tmp\", \
              \"content\": \"subsystem: update related contract\\n\\nBody explaining the fix.\\n\\nAssisted-by: kres:test\\n\", \
              \"purpose\": \"commit message\"}]}",
        ),
    ]);
    responses.extend(clean_review_responses(&workflow));
    let port = spawn_mock(responses).await;
    let mut inputs = Map::new();
    inputs.insert("target".into(), Value::String("freeform bug prose".into()));
    inputs.insert("assisted_by".into(), Value::String("kres:test".into()));
    let inputs = derive_inputs(&workflow, inputs);

    let (_guard, workspace) = fresh_git_repo();
    let mut driver = LlmDriver::new(workspace.clone(), workflow.clone())
        .with_fast(fast_env_pointing_at(port))
        .with_slow(slow_env_pointing_at(port))
        .with_code(slow_env_pointing_at(port));

    let trace = run(&workflow, &mut driver, inputs).await;
    eprintln!("{}", trace.pretty());
    assert!(
        matches!(
            trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ),
        "expected workflow success, got {:?}",
        trace.status
    );

    let produced =
        |id: &str| -> Map<String, Value> {
            trace
                .events
                .iter()
                .find_map(|e| match e {
                    kres_agents::workflow_exec::TraceEvent::StepProduced {
                        id: i, outputs, ..
                    } if i == id => Some(outputs.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };

    let wp = produced("write-patch");
    assert_eq!(wp.get("changed_files"), Some(&json!(["b.h"])));
    assert_eq!(wp.get("code_changes_emitted"), Some(&json!(true)));
    assert_eq!(wp.get("affected_files_changed"), Some(&json!(true)));

    let out = std::process::Command::new("git")
        .args(["show", "HEAD:b.h"])
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "#define B 2\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_patch_review_retry_includes_previous_git_diff_context() {
    let workflow = parse_workflow(include_str!("../../configs/workflows/fix.json")).unwrap();
    let mut responses = VecDeque::from(vec![
        confirmed_research_response("Research found the file.", &["a.c"], &["f"], "Change a.c."),
        empty_lore_search_response(),
        fake_messages_response(
            "First patch.\n\
             {\"build_target\": \"a.o\", \
              \"code_edits\": [{\"file_path\": \"a.c\", \
              \"old_string\": \"int x = 1;\\n\", \
              \"new_string\": \"int x = 2;\\n\"}]}",
        ),
        fake_messages_response(
            "No proven introducing commit.\n\
             {\"fixes_sha\": \"\", \
              \"fixes_subject\": \"\", \
              \"fixes_evidence\": \"\", \
              \"unproven_fixes_candidates\": [], \
              \"analysis\": \"No relevant provenance in toy repo.\"}",
        ),
        fake_messages_response(
            "First commit message.\n\
             {\"code_output\": [{\"path\": \".kres-commit-msg.tmp\", \
              \"content\": \"subsystem: first patch\\n\\nBody explaining the first fix.\\n\\nAssisted-by: kres:test\\n\", \
              \"purpose\": \"commit message\"}]}",
        ),
    ]);
    responses.extend(dirty_source_review_responses(&workflow));
    responses.push_back(review_ledger_response("source", "open"));
    responses.push_back(orchestrator_picks("write-patch"));
    responses.push_back(fake_messages_response(
        "Corrected patch.\n\
         {\"build_target\": \"a.o\", \
          \"code_edits\": [{\"file_path\": \"a.c\", \
          \"old_string\": \"int x = 2;\\n\", \
          \"new_string\": \"int x = 3;\\n\"}]}",
    ));
    responses.push_back(review_ledger_response("source", "addressed"));
    // fixes-tag-search is skipped on the orchestrator-driven retry
    // (its run_if pins it to attempt == 0), so the next mock response
    // consumed after write-patch attempt 2 is the commit-message
    // response — no spurious fixes-tag-search response queued.
    responses.push_back(fake_messages_response(
        "Second commit message.\n\
         {\"code_output\": [{\"path\": \".kres-commit-msg.tmp\", \
          \"content\": \"subsystem: corrected patch\\n\\nBody explaining the corrected fix.\\n\\nAssisted-by: kres:test\\n\", \
          \"purpose\": \"commit message\"}]}",
    ));
    responses.extend(clean_review_responses(&workflow));
    responses.push_back(review_ledger_response("source", "resolved"));
    let (port, requests) = spawn_recording_mock(responses).await;

    let mut inputs = Map::new();
    inputs.insert("target".into(), Value::String("freeform bug prose".into()));
    inputs.insert("assisted_by".into(), Value::String("kres:test".into()));
    let inputs = derive_inputs(&workflow, inputs);

    let (_guard, workspace) = fresh_git_repo();
    let mut driver = LlmDriver::new(workspace.clone(), workflow.clone())
        .with_fast(fast_env_pointing_at(port))
        .with_slow(slow_env_pointing_at(port))
        .with_code(slow_env_pointing_at(port));

    let trace = run(&workflow, &mut driver, inputs).await;
    eprintln!("{}", trace.pretty());
    assert!(
        matches!(
            trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ),
        "expected workflow success, got {:?}",
        trace.status
    );

    let requests = requests.lock().await.join("\n---REQUEST---\n");
    assert!(
        requests.contains("--- PREVIOUS PATCH FROM `git diff HEAD~1` ---"),
        "write-patch correction prompt did not include previous-patch block"
    );
    assert!(
        requests.contains("exact output from `git diff HEAD~1`"),
        "previous-patch block did not label command provenance"
    );
    assert!(
        requests.contains("KRES-READONLY| diff --git a/a.c b/a.c"),
        "previous patch diff was not inlined as read-only payload"
    );

    let out = std::process::Command::new("git")
        .args(["show", "HEAD:a.c"])
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "int x = 3;\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_message_review_retry_includes_old_message_and_patch_context() {
    let workflow = parse_workflow(include_str!("../../configs/workflows/fix.json")).unwrap();
    let mut responses = VecDeque::from(vec![
        confirmed_research_response("Research found the file.", &["a.c"], &["f"], "Change a.c."),
        empty_lore_search_response(),
        fake_messages_response(
            "Patch.\n\
             {\"build_target\": \"a.o\", \
              \"code_edits\": [{\"file_path\": \"a.c\", \
              \"old_string\": \"int x = 1;\\n\", \
              \"new_string\": \"int x = 2;\\n\"}]}",
        ),
        fake_messages_response(
            "No proven introducing commit.\n\
             {\"fixes_sha\": \"\", \
              \"fixes_subject\": \"\", \
              \"fixes_evidence\": \"\", \
              \"unproven_fixes_candidates\": [], \
              \"analysis\": \"No relevant provenance in toy repo.\"}",
        ),
        fake_messages_response(
            "First commit message.\n\
             {\"code_output\": [{\"path\": \".kres-commit-msg.tmp\", \
              \"content\": \"subsystem: stale claim\\n\\nBody with a stale claim.\\n\\nAssisted-by: kres:test\\n\", \
              \"purpose\": \"commit message\"}]}",
        ),
    ]);
    responses.extend(dirty_commit_message_review_responses(&workflow));
    responses.push_back(review_ledger_response("commit_message", "open"));
    responses.push_back(orchestrator_picks("write-commit-message"));
    responses.extend(vec![fake_messages_response(
        "Rewritten commit message.\n\
         {\"code_output\": [{\"path\": \".kres-commit-msg.tmp\", \
          \"content\": \"subsystem: corrected claim\\n\\nBody with the corrected claim.\\n\\nAssisted-by: kres:test\\n\", \
          \"purpose\": \"commit message\"}]}",
    )]);
    responses.push_back(review_ledger_response("commit_message", "addressed"));
    responses.extend(clean_review_responses(&workflow));
    responses.push_back(review_ledger_response("commit_message", "resolved"));
    let (port, requests) = spawn_recording_mock(responses).await;

    let mut inputs = Map::new();
    inputs.insert("target".into(), Value::String("freeform bug prose".into()));
    inputs.insert("assisted_by".into(), Value::String("kres:test".into()));
    let inputs = derive_inputs(&workflow, inputs);

    let (_guard, workspace) = fresh_git_repo();
    let mut driver = LlmDriver::new(workspace.clone(), workflow.clone())
        .with_fast(fast_env_pointing_at(port))
        .with_slow(slow_env_pointing_at(port))
        .with_code(slow_env_pointing_at(port));

    let trace = run(&workflow, &mut driver, inputs).await;
    eprintln!("{}", trace.pretty());
    assert!(
        matches!(
            trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ),
        "expected workflow success, got {:?}",
        trace.status
    );

    let requests = requests.lock().await.join("\n---REQUEST---\n");
    assert!(
        requests.contains("--- CURRENT COMMITTED PATCH CONTEXT FOR COMMIT MESSAGE REWRITE ---"),
        "commit-message correction prompt did not include current committed context block"
    );
    assert!(
        requests.contains("exact output from `git log -1 --format=%B`"),
        "commit-message block did not label old message provenance"
    );
    assert!(
        requests.contains("KRES-READONLY| subsystem: stale claim"),
        "old commit message was not inlined as read-only payload"
    );
    assert!(
        requests.contains("KRES-READONLY| diff --git a/a.c b/a.c"),
        "current patch diff was not inlined as read-only payload"
    );
    assert!(
        requests.contains("That parent already includes earlier todos in the series"),
        "commit-message prompt did not explain series parent semantics"
    );
    assert!(
        requests.contains("Do not paste stale pre-series source snippets"),
        "commit-message prompt did not forbid stale pre-series snippets"
    );

    let out = std::process::Command::new("git")
        .args(["log", "-1", "--format=%s"])
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "subsystem: corrected claim"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_declared_output_retries_then_surfaces_as_driver_error() {
    // Mock returns JSON without the required `valid` key — the
    // runner's extract_outputs should reject it. It retries the LLM
    // call with the JSON-required prefix, then the research eval
    // budget repeats the whole step twice more before the final
    // output-extraction driver error surfaces.
    let responses = VecDeque::from(
        (0..12)
            .map(|i| fake_messages_response(&format!("Still wrong. {{\"unrelated_field\": {i}}}")))
            .collect::<Vec<_>>(),
    );
    let port = spawn_mock(responses).await;

    let workflow = parse_workflow(include_str!("../../configs/workflows/fix.json")).unwrap();
    let mut inputs = Map::new();
    inputs.insert("target".into(), Value::String("prose target".into()));
    let inputs = derive_inputs(&workflow, inputs);

    let workspace = std::env::temp_dir();
    let mut driver = LlmDriver::new(workspace, workflow.clone())
        .with_fast(fast_env_pointing_at(port))
        .with_slow(slow_env_pointing_at(port));

    let trace = run(&workflow, &mut driver, inputs).await;
    eprintln!("{}", trace.pretty());
    match &trace.status {
        WorkflowStatus::Failure(msg) => {
            assert!(
                msg.contains("research") && msg.contains("attempt 3"),
                "expected final research-step retry error, got: {msg}"
            );
            assert!(
                msg.contains("output extraction") || msg.contains("declared key"),
                "expected output-extraction error, got: {msg}"
            );
        }
        other => panic!("expected Failure, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_json_response_is_retried_before_failure() {
    let responses = VecDeque::from(vec![
        fake_messages_response("I forgot the required object."),
        invalid_research_response(
            "Corrected.",
            "drivers/example/example.c: cleanup already happens",
            "not a bug",
        ),
    ]);
    let port = spawn_mock(responses).await;

    let workflow = parse_workflow(include_str!("../../configs/workflows/fix.json")).unwrap();
    let mut inputs = Map::new();
    inputs.insert("target".into(), Value::String("prose target".into()));
    let inputs = derive_inputs(&workflow, inputs);

    let workspace = std::env::temp_dir();
    let mut driver = LlmDriver::new(workspace, workflow.clone())
        .with_fast(fast_env_pointing_at(port))
        .with_slow(slow_env_pointing_at(port));

    let trace = run(&workflow, &mut driver, inputs).await;
    eprintln!("{}", trace.pretty());
    assert!(matches!(trace.status, WorkflowStatus::TerminalSuccess(_)));
    assert!(trace.events.iter().any(|e| matches!(
        e,
        kres_agents::workflow_exec::TraceEvent::StepProduced { id, outputs, .. }
            if id == "research" && outputs.get("valid") == Some(&json!(false))
                && outputs.get("research_status") == Some(&json!("invalid"))
    )));
    assert!(!trace.events.iter().any(|e| matches!(
        e,
        kres_agents::workflow_exec::TraceEvent::StepProduced { id, .. }
            if id == "write-patch"
    )));
}

/// Regression for the user-reported bug:
/// `kres --results may6 --prompt '/review HEAD'` exited with no
/// artefacts in `may6/`. The CLI's --prompt short-circuit dropped
/// --results, and the workflow runner had no concept of writing
/// findings.json / report.md.
///
/// This test runs the review workflow against the fake LLM,
/// invokes write_workflow_artefacts on the resulting trace, and
/// asserts the operator's two expected files land in the chosen
/// results dir.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn results_dir_gets_findings_and_report() {
    // Build a minimal one-step workflow (no lensed gather — keeps
    // the mock-LLM payload count down to 1 response). Declares a
    // `findings` array output so write_workflow_artefacts has
    // something to fold into findings.json.
    let wf_json = serde_json::json!({
        "$schema_version": 1,
        "id": "review-results-test",
        "steps": [{
            "id": "scan",
            "agent": "fast",
            "prompt": "scan the target",
            "outputs": {
                "findings": {"type": "array<object>"},
                "analysis": {"type": "string"}
            }
        }]
    });
    let workflow = kres_agents::workflow::parse_workflow(&wf_json.to_string()).unwrap();

    // The fake LLM emits TWO findings + an analysis blurb. The
    // runner's extract_outputs picks the trailing JSON object.
    let responses = VecDeque::from(vec![fake_messages_response(
        "I traced two bugs. Here is the structured payload:\n\
         {\"findings\": [\
           {\"file\": \"net/foo.c:10\", \"what\": \"leak\", \"severity\": \"high\"},\
           {\"file\": \"fs/bar.c:42\", \"what\": \"uaf\",  \"severity\": \"high\"}\
         ], \"analysis\": \"two refcount bugs in foo and bar\"}",
    )]);
    let port = spawn_mock(responses).await;

    let workspace = std::env::temp_dir();
    let mut driver = kres_agents::workflow_runner::LlmDriver::new(workspace, workflow.clone())
        .with_fast(fast_env_pointing_at(port));

    let inputs = Map::new();
    let trace = kres_agents::workflow_exec::run(&workflow, &mut driver, inputs).await;
    eprintln!("{}", trace.pretty());
    assert!(matches!(
        trace.status,
        kres_agents::workflow_exec::WorkflowStatus::Success
            | kres_agents::workflow_exec::WorkflowStatus::TerminalSuccess(_)
    ));

    // Now exercise write_workflow_artefacts and check the files
    // landed in the operator-chosen --results dir.
    let results = tempfile::tempdir().unwrap();
    let written =
        kres_agents::workflow_runner::write_workflow_artefacts(results.path(), &workflow, &trace)
            .expect("write_workflow_artefacts");
    assert_eq!(written.len(), 2, "findings.json + report.md");

    let findings_path = results.path().join("findings.json");
    let report_path = results.path().join("report.md");
    assert!(findings_path.exists(), "findings.json must exist");
    assert!(report_path.exists(), "report.md must exist");

    let findings_body = std::fs::read_to_string(&findings_path).unwrap();
    let findings: Value = serde_json::from_str(&findings_body).unwrap();
    let arr = findings.as_array().expect("findings.json is an array");
    assert_eq!(arr.len(), 2, "two findings persisted");
    let what0 = arr[0].get("what").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        what0.contains("leak"),
        "first finding kept its `what`: {what0}"
    );

    let report_body = std::fs::read_to_string(&report_path).unwrap();
    assert!(
        report_body.contains("# kres workflow run: review-results-test"),
        "report has the workflow id"
    );
    assert!(
        report_body.contains("Findings: 2"),
        "report counts findings: {report_body}"
    );
    assert!(
        report_body.contains("## Findings"),
        "report renders findings: {report_body}"
    );
    assert!(
        report_body.contains("net/foo.c:10") && report_body.contains("leak"),
        "report includes finding details, not just a count: {report_body}"
    );
    assert!(
        report_body.contains("two refcount bugs in foo and bar"),
        "report includes the analysis prose"
    );
}

/// Edge case: a workflow that produces NO findings still writes
/// report.md (so the operator gets *something* visible) and skips
/// findings.json (don't write an empty array — confusing).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn results_dir_no_findings_writes_report_only() {
    let wf_json = serde_json::json!({
        "$schema_version": 1,
        "id": "empty-findings",
        "steps": [{
            "id": "scan",
            "agent": "fast",
            "prompt": "p",
            "outputs": {"findings": {"type": "array<object>"}}
        }]
    });
    let workflow = kres_agents::workflow::parse_workflow(&wf_json.to_string()).unwrap();
    let responses = VecDeque::from(vec![fake_messages_response(
        "nothing to see\n{\"findings\": []}",
    )]);
    let port = spawn_mock(responses).await;

    let workspace = std::env::temp_dir();
    let mut driver = kres_agents::workflow_runner::LlmDriver::new(workspace, workflow.clone())
        .with_fast(fast_env_pointing_at(port));
    let trace = kres_agents::workflow_exec::run(&workflow, &mut driver, Map::new()).await;
    let results = tempfile::tempdir().unwrap();
    let written =
        kres_agents::workflow_runner::write_workflow_artefacts(results.path(), &workflow, &trace)
            .unwrap();
    // Only report.md.
    assert_eq!(written.len(), 1);
    assert!(written[0].ends_with("report.md"));
    assert!(!results.path().join("findings.json").exists());
}

/// Regression for the doubled-path bug:
/// `<workspace>/.kres/logs/.kres/logs/<uuid>` was the actual log
/// dir because run_workflow pre-joined `.kres/logs` and then
/// TurnLogger::new joined another `.kres/logs/<uuid>` on top.
/// The fix passes a base dir; verify TurnLogger::new sets the
/// session dir to `<base>/.kres/logs/<uuid>` exactly.
#[test]
fn turn_logger_session_dir_does_not_double() {
    let tmp = tempfile::tempdir().unwrap();
    // Pass the bare workspace as the base — TurnLogger::new
    // appends `.kres/logs/<uuid>` itself. Earlier broken code
    // passed `<workspace>/.kres/logs` which produced doubled
    // paths.
    let lg = kres_core::log::TurnLogger::new(tmp.path()).unwrap();
    let session = lg.session_dir().to_path_buf();

    let session_str = session.display().to_string();
    let kres_logs_count = session_str.matches(".kres/logs/").count()
        + session_str.matches(".kres/logs").count()
        - session_str.matches(".kres/logs/").count(); // bare dir count
    let _ = kres_logs_count; // sanity placeholder; below is the real check.

    // Strict check: the suffix after the workspace should be
    // exactly `.kres/logs/<uuid>` — one occurrence of `.kres/logs`,
    // not two.
    let stripped = session
        .strip_prefix(tmp.path())
        .expect("session dir lives under tmp workspace");
    let comps: Vec<_> = stripped.components().collect();
    assert_eq!(
        comps.len(),
        3,
        "expected `.kres/logs/<uuid>` (3 components), got {:?}",
        comps
    );
    assert_eq!(comps[0].as_os_str(), ".kres");
    assert_eq!(comps[1].as_os_str(), "logs");
    // Third component is the session uuid; just check it parses
    // as something non-empty.
    assert!(!comps[2].as_os_str().is_empty());
}
