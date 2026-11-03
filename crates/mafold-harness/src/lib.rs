//! Mafold Harness — ONE tool loop for every Mafold model bot.
//!
//! Contract: `.docs/native-harness-v1.md`. The loop owns the turn: it asks the
//! model, runs the tools the model calls, folds in what people say while it
//! works, honours Stop, and writes the reply as the same cards every other
//! runtime's turns render as. It ships **zero builtin tools and zero prompt
//! words** — personas, tools and commands all arrive as [`HarnessPlugin`]s,
//! and a bot is just a (runtime × plugins) profile.
//!
//! Two hosts run it. mafold-api: the official bots (claude, gemini, chatgpt,
//! deepseek), the Mafold assistant and every Cloud agent. mafold-cli: a
//! Mafold Harness agent on someone's own computer. What differs between them
//! is only what a host hands the loop — the history it built, where Stop and
//! mid-turn messages come from ([`kernel::Live`]), who settles the bill
//! ([`kernel::Settle`]) — never the loop itself.

pub mod fields;
pub mod kernel;
pub mod plugin;
pub mod runtime;

pub use fields::{FieldOption, FieldSpec};
pub use kernel::{Bill, Frame, Live, Settle, TokenUsage, Tools, Turn};
pub use plugin::{HarnessPlugin, PersonaPlugin, Prepared, ToolSpec, TurnCtx};
pub use runtime::{
    default_reasoning_effort, unrunnable_call, ChatEvent, ModelRuntime, OpenAiCompatRuntime, ToolCall,
    TurnMsg, EFFORT_BY_MODEL, NO_VISIBLE_OUTPUT, OUTPUT_CAP_NO_ANSWER,
};
