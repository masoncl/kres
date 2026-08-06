//! Agent roles: fast, slow, main, todo, consolidator.
//!
//! Phase 4 landed: agent configs, response parsing (prose-then-JSON
//! strict serde response contracts, followup types,
//! prompt builders. The actual fast/slow pipeline runner is a follow-
//! on phase.
//!
//! The cross-task findings merger (LLM-based whole-list rewrite)
//! was retired in favour of deterministic delta application in
//! `kres_core::findings::apply_delta_to_list`.

pub mod config;
pub mod consolidate;
pub mod embedded_prompts;
pub mod error;
pub mod fetcher;
pub mod finding_repair;
pub mod followup;
pub mod goal;
pub mod json_repair;
pub mod main_agent;
pub mod mcp_fetcher;
pub mod pipeline;
pub mod prioritize;
pub mod promote;
pub mod prompt;
pub mod prompt_file;
pub mod response;
pub mod skills;
pub mod symbol;
pub mod todo_agent;
pub mod tools;
pub mod user_commands;
pub mod workflow;
pub mod workflow_exec;
pub mod workflow_runner;
pub mod workspace;

pub use config::{AgentConfig, AgentKind};
pub use consolidate::{consolidate_lenses, ConsolidatedTask, LensOutput};
pub use error::AgentError;
pub use fetcher::{parse_read_spec, WorkspaceFetcher};
pub use followup::Followup;
pub use goal::{
    check_goal, define_goal, define_plan, GoalCheck, GoalClient, GoalDefinition, GOAL_INSTRUCTIONS,
};
pub use kres_core::TaskMode;
pub use main_agent::{parse_actions, MainAgent, DEFAULT_MAX_MAIN_TURNS};
pub use mcp_fetcher::{McpFetcher, McpMethodMap};
pub use pipeline::{
    AgentRunner, ConsolidatorClient, DataFetcher, FetchResult, NullFetcher, RunContext, TaskSummary,
};
pub use prioritize::{prioritize_pending_with_logger, PrioritizeClient, PrioritizeInputs};
pub use prompt_file::{parse as parse_prompt_file, PromptFile};
pub use response::CodeResponse;
pub use skills::{InvocationPolicy, Skill, Skills};
pub use symbol::{
    append_context, append_prompt_evidence, append_symbol, canonical_semcode_evidence,
    canonicalize_prompt_evidence, parse_semcode_symbol, propagate_tool_result, tool_source,
    with_retrieval_source, SemcodeEvidence,
};
pub use todo_agent::{
    dedup_tokens, parse_todo_response, update_todo_via_agent, update_todo_via_agent_with_logger,
    TodoAgentInputs, TodoClient, TodoUpdate, TODO_INSTRUCTIONS,
};
pub use workspace::{detect_workspace, BuildSystem, WorkspaceKind, WorkspaceProfile};
