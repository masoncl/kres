//! Agent roles: fast, slow, main, todo, consolidator, merger.
//!
//! Phase 4 landed: agent configs, response parsing (prose-then-JSON
//! fallback, fenced-block extraction, brace-match), followup types,
//! prompt builders. The actual fast/slow pipeline runner is a follow-
//! on phase.

pub mod config;
pub mod consolidate;
pub mod error;
pub mod fetcher;
pub mod followup;
pub mod goal;
pub mod main_agent;
pub mod mcp_fetcher;
pub mod merge;
pub mod pipeline;
pub mod prompt;
pub mod prompt_file;
pub mod response;
pub mod skills;
pub mod symbol;
pub mod todo_agent;
pub mod tools;

pub use config::{AgentConfig, AgentKind};
pub use consolidate::{consolidate_lenses, ConsolidatedTask, LensOutput};
pub use error::AgentError;
pub use fetcher::{parse_read_spec, WorkspaceFetcher};
pub use followup::Followup;
pub use goal::{
    check_goal, define_goal, GoalCheck, GoalClient, GoalDefinition, GOAL_INSTRUCTIONS,
};
pub use kres_core::TaskMode;
pub use main_agent::{parse_actions, MainAgent, DEFAULT_MAX_MAIN_TURNS};
pub use mcp_fetcher::{McpFetcher, McpMethodMap};
pub use merge::{merge_findings, MERGER_SYSTEM};
pub use pipeline::{
    ConsolidatorClient, DataFetcher, FetchResult, NullFetcher, Orchestrator, RunContext,
    TaskSummary,
};
pub use prompt_file::{parse as parse_prompt_file, PromptFile};
pub use response::{parse_code_response, CodeEdit, CodeResponse};
pub use skills::{InvocationPolicy, Skill, Skills};
pub use symbol::{
    append_context, append_symbol, ctx_identity, parse_semcode_symbol, previously_fetched_manifest,
    propagate_tool_result, sym_identity, tool_source,
};
pub use todo_agent::{
    dedup_tokens, extract_citations, parse_todo_response, update_todo_via_agent,
    update_todo_via_agent_with_logger, TodoClient,
};
