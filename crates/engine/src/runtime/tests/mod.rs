//! Engine runtime integration tests, grouped by topic.

pub(crate) mod support;

mod agents;
mod approvals;
mod artifacts;
mod compaction;
mod delegation;
mod delegation_queue;
mod delegation_revert;
mod goal_reminders;
mod goal_selection;
mod mailbox;
mod messaging;
mod misc;
mod model_loop;
mod model_policy;
mod permissions;
mod plugins;
mod producers;
mod producers_discard;
mod producers_plugin;
mod prompts;
mod providers;
mod recovery;
mod replay;
mod sessions;
mod skills;
mod subagent_control;
mod subagent_residency;
mod subagent_resume;
mod subagents;
mod tool_execution;
mod usage;
