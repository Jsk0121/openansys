use crate::skills::{SkillMetadata, render_skills_section};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

const DEFAULT_TOKEN_LIMIT: u64 = 128 * 1024;
const GPT5_TOKEN_LIMIT: u64 = 1_000_000;
const COMPACTION_TRIGGER_NUMERATOR: u64 = 4;
const COMPACTION_TRIGGER_DENOMINATOR: u64 = 5;
const HISTORY_SUMMARY_PREFIX: &str = "[history summary]";

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct HistoryFile {
    pub(crate) version: u32,
    pub(crate) session_id: String,
    pub(crate) workspace_root: String,
    pub(crate) last_active_at_ms: u128,
    #[serde(default)]
    pub(crate) total_input_tokens: u64,
    #[serde(default)]
    pub(crate) total_output_tokens: u64,
    #[serde(default)]
    pub(crate) total_tokens: u64,
    pub(crate) entries: Vec<HistoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum HistoryEntry {
    System {
        content: String,
        #[serde(default)]
        estimated_tokens: u64,
    },
    User {
        content: String,
        #[serde(default)]
        estimated_tokens: u64,
    },
    Assistant {
        content: String,
        #[serde(default)]
        reasoning_content: String,
        #[serde(default)]
        tool_calls: Vec<AssistantToolCall>,
        #[serde(default)]
        estimated_tokens: u64,
    },
    Tool {
        #[serde(default)]
        tool_call_id: String,
        #[serde(default)]
        tool_name: String,
        content: String,
        #[serde(default)]
        estimated_tokens: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AssistantToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionMode {
    BeforeTurn,
    MidTurn,
}

impl HistoryFile {
    pub(crate) fn push_user(&mut self, content: String) {
        let mut entry = HistoryEntry::User {
            content,
            estimated_tokens: 0,
        };
        entry.set_estimated_tokens(entry.weight().saturating_mul(4));
        self.entries.push(entry);
    }

    pub(crate) fn push_assistant(
        &mut self,
        content: String,
        reasoning_content: String,
        tool_calls: Vec<AssistantToolCall>,
    ) {
        let mut entry = HistoryEntry::Assistant {
            content,
            reasoning_content,
            tool_calls,
            estimated_tokens: 0,
        };
        entry.set_estimated_tokens(entry.weight().saturating_mul(4));
        self.entries.push(entry);
    }

    pub(crate) fn push_tool(&mut self, tool_call_id: String, tool_name: String, content: String) {
        let mut entry = HistoryEntry::Tool {
            tool_call_id,
            tool_name,
            content,
            estimated_tokens: 0,
        };
        entry.set_estimated_tokens(entry.weight().saturating_mul(4));
        self.entries.push(entry);
    }

    pub(crate) fn push_system(&mut self, content: String) {
        let mut entry = HistoryEntry::System {
            content,
            estimated_tokens: 0,
        };
        entry.set_estimated_tokens(entry.weight().saturating_mul(4));
        self.entries.push(entry);
    }

    pub(crate) fn note_api_usage(
        &mut self,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        total_tokens: Option<u64>,
    ) {
        let start = self.last_system_index().unwrap_or(0);
        let entry_count = self.entries[start..].len() as u64;
        let entry_estimate = estimate_entry_tokens(&self.entries, start);
        self.total_input_tokens = self
            .total_input_tokens
            .saturating_add(input_tokens.unwrap_or(0));
        self.total_output_tokens = self
            .total_output_tokens
            .saturating_add(output_tokens.unwrap_or(0));
        let fallback_total = input_tokens
            .unwrap_or(0)
            .saturating_add(output_tokens.unwrap_or(0));
        self.total_tokens = self
            .total_tokens
            .saturating_add(total_tokens.unwrap_or(fallback_total));

        let active_estimate = input_tokens
            .or(total_tokens)
            .unwrap_or_else(|| entry_estimate.max(entry_count));
        apply_estimated_tokens(&mut self.entries, start, active_estimate);
    }

    pub(crate) fn active_token_usage(&self) -> u64 {
        let start = self.last_system_index().unwrap_or(0);
        self.entries[start..]
            .iter()
            .map(HistoryEntry::estimated_tokens)
            .sum()
    }

    pub(crate) fn total_token_usage(&self) -> u64 {
        self.total_tokens
    }

    pub(crate) fn needs_compaction(&self, token_limit: u64) -> bool {
        self.active_token_usage()
            >= token_limit.saturating_mul(COMPACTION_TRIGGER_NUMERATOR)
                / COMPACTION_TRIGGER_DENOMINATOR
    }

    pub(crate) fn last_user_content(&self) -> Option<String> {
        self.entries.iter().rev().find_map(|entry| match entry {
            HistoryEntry::User { content, .. } => Some(content.clone()),
            _ => None,
        })
    }

    pub(crate) fn compaction_prompt(&self, mode: CompactionMode) -> String {
        match mode {
            CompactionMode::BeforeTurn => concat!(
                "对话已接近 token 上限。简要总结之前对后续工作有用的历史记录。",
                "重点关注持久上下文、关键决策、重要文件、约束条件和未解决的问题。"
            )
            .to_string(),
            CompactionMode::MidTurn => concat!(
                "对话已接近 token 上限且当前任务尚未完成。",
                "简要总结之前对继续工作有用的历史记录。",
                "包括当前任务、已完成的工作、",
                "重要文件和约束条件、最近的工具结果以及最有用的下一步。",
                ""
            )
            .to_string(),
        }
    }

    pub(crate) fn apply_compaction(&mut self, summary: String, resume_user: Option<String>) {
        self.push_system(format!("{HISTORY_SUMMARY_PREFIX}\n{}", summary.trim()));
        if let Some(user) = resume_user {
            self.push_user(user);
        }
        let start = self.last_system_index().unwrap_or(0);
        let estimated = estimate_entry_tokens(&self.entries, start);
        apply_estimated_tokens(&mut self.entries, start, estimated.max(1));
    }

    fn last_system_index(&self) -> Option<usize> {
        self.entries
            .iter()
            .rposition(|entry| matches!(entry, HistoryEntry::System { .. }))
    }
}

impl HistoryEntry {
    pub(crate) fn estimated_tokens(&self) -> u64 {
        match self {
            Self::System {
                estimated_tokens, ..
            }
            | Self::User {
                estimated_tokens, ..
            }
            | Self::Assistant {
                estimated_tokens, ..
            }
            | Self::Tool {
                estimated_tokens, ..
            } => *estimated_tokens,
        }
    }

    fn weight(&self) -> u64 {
        match self {
            Self::System { content, .. }
            | Self::User { content, .. }
            | Self::Assistant { content, .. } => {
                let tool_call_weight = match self {
                    Self::Assistant {
                        reasoning_content,
                        tool_calls,
                        ..
                    } => {
                        text_weight(reasoning_content)
                            + tool_calls
                                .iter()
                                .map(|call| text_weight(&call.name) + text_weight(&call.arguments))
                                .sum::<u64>()
                    }
                    _ => 0,
                };
                text_weight(content) + tool_call_weight
            }
            Self::Tool { content, .. } => text_weight(content),
        }
    }

    fn set_estimated_tokens(&mut self, value: u64) {
        match self {
            Self::System {
                estimated_tokens, ..
            }
            | Self::User {
                estimated_tokens, ..
            }
            | Self::Assistant {
                estimated_tokens, ..
            }
            | Self::Tool {
                estimated_tokens, ..
            } => *estimated_tokens = value,
        }
    }

    fn reset_estimated_tokens(&mut self) {
        match self {
            Self::System {
                estimated_tokens, ..
            }
            | Self::User {
                estimated_tokens, ..
            }
            | Self::Assistant {
                estimated_tokens, ..
            }
            | Self::Tool {
                estimated_tokens, ..
            } => *estimated_tokens = 0,
        }
    }

    fn add_estimated_tokens(&mut self, delta: u64) {
        match self {
            Self::System {
                estimated_tokens, ..
            }
            | Self::User {
                estimated_tokens, ..
            }
            | Self::Assistant {
                estimated_tokens, ..
            }
            | Self::Tool {
                estimated_tokens, ..
            } => *estimated_tokens = estimated_tokens.saturating_add(delta),
        }
    }
}

pub(crate) fn build_messages(
    workspace_root: &Path,
    skills: &[SkillMetadata],
    enable_shell_tool: bool,
    entries: &[HistoryEntry],
) -> Vec<Value> {
    let mut messages = vec![json!({
        "role": "system",
        "content": system_prompt(workspace_root, skills, enable_shell_tool)
    })];

    let active_entries = match entries
        .iter()
        .rposition(|entry| matches!(entry, HistoryEntry::System { .. }))
    {
        Some(index) => &entries[index..],
        None => entries,
    };

    for entry in active_entries {
        match entry {
            HistoryEntry::System { content, .. } => {
                messages.push(json!({"role": "system", "content": content}));
            }
            HistoryEntry::User { content, .. } => {
                messages.push(json!({"role": "user", "content": content}));
            }
            HistoryEntry::Assistant {
                content,
                reasoning_content,
                tool_calls,
                ..
            } => {
                let assistant_content = if tool_calls.is_empty() || !content.trim().is_empty() {
                    Value::String(content.clone())
                } else {
                    Value::Null
                };
                let mut message = json!({"role": "assistant", "content": assistant_content});
                if !tool_calls.is_empty() {
                    message["tool_calls"] = Value::Array(
                        tool_calls
                            .iter()
                            .map(|call| {
                                json!({
                                    "id": call.id,
                                    "type": "function",
                                    "function": {
                                        "name": call.name,
                                        "arguments": call.arguments
                                    }
                                })
                            })
                            .collect(),
                    );
                }
                if !reasoning_content.trim().is_empty() {
                    message["reasoning_content"] = Value::String(reasoning_content.clone());
                }
                messages.push(message);
            }
            HistoryEntry::Tool {
                tool_call_id,
                tool_name,
                content,
                ..
            } => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "name": tool_name,
                    "content": content
                }));
            }
        }
    }

    messages
}

pub(crate) fn token_limit_for_model(model: &str) -> u64 {
    let normalized = model.trim().to_ascii_lowercase();
    if normalized.starts_with("gpt-5") {
        GPT5_TOKEN_LIMIT
    } else if normalized.starts_with("deepseek-v3.2") {
        DEFAULT_TOKEN_LIMIT
    } else {
        DEFAULT_TOKEN_LIMIT
    }
}

fn apply_estimated_tokens(entries: &mut [HistoryEntry], start: usize, target_total: u64) {
    if start >= entries.len() {
        return;
    }

    for entry in entries.iter_mut() {
        entry.reset_estimated_tokens();
    }

    distribute_tokens(&mut entries[start..], target_total);
}

fn estimate_entry_tokens(entries: &[HistoryEntry], start: usize) -> u64 {
    if start >= entries.len() {
        return 0;
    }

    let weight = entries[start..]
        .iter()
        .map(HistoryEntry::weight)
        .sum::<u64>();
    weight.saturating_mul(4)
}

fn distribute_tokens(entries: &mut [HistoryEntry], delta: u64) {
    if entries.is_empty() || delta == 0 {
        return;
    }

    let total_weight = entries.iter().map(HistoryEntry::weight).sum::<u64>();
    if total_weight == 0 {
        let base = delta / entries.len() as u64;
        let mut remainder = delta % entries.len() as u64;
        for entry in entries {
            let extra = u64::from(remainder > 0);
            entry.add_estimated_tokens(base + extra);
            remainder = remainder.saturating_sub(1);
        }
        return;
    }

    let mut assigned = 0_u64;
    let last_index = entries.len() - 1;
    for (index, entry) in entries.iter_mut().enumerate() {
        let share = if index == last_index {
            delta.saturating_sub(assigned)
        } else {
            delta.saturating_mul(entry.weight()) / total_weight
        };
        assigned = assigned.saturating_add(share);
        entry.add_estimated_tokens(share);
    }
}

fn text_weight(text: &str) -> u64 {
    text.split_whitespace().count().max(1) as u64
}

fn system_prompt(
    workspace_root: &Path,
    skills: &[SkillMetadata],
    enable_shell_tool: bool,
) -> String {
    let mut prompt = format!(
        concat!(
            "你是 Mini Codex，一个工作在本地工作区内的编程助手。\n",
            "工作区根目录：{}。\n",
            "回复应简洁，专注于完成用户的请求。\n",
            "在修改代码或回答关于本地项目的问题之前，优先检查当前状态。\n",
            "除非明确必要，否则不要使用破坏性命令，永远不要访问工作区根目录之外的任何内容。\n",
            "当需要检查文件、编辑代码、运行程序、搜索、调试、构建、测试、格式化、git 操作或其他工作区任务时，使用可用的 shell_tool。\n",
            "将 shell_tool 视为强大且潜在危险的工具。\n",
            "如果 shell 命令失败，仔细阅读结果并尝试其他有效方法。\n",
            "调用 shell_tool 时，workdir 参数是可选的且必须位于工作区根目录内。\n",
            "优先每次只调用一个 shell_tool，除非批量执行多个独立的只读命令明显更有效率。\n",
            "在调用工具之前，通常应包含简短的前言，说明你即将做什么以及为什么。\n",
            "工具前言应简短：通常一句话，不超过 20 个词。\n",
            "如果不需要 shell 访问，则正常回答。\n"
        ),
        workspace_root.display()
    );

    if enable_shell_tool {
        if let Some(skills_section) = render_skills_section(skills) {
            prompt.push('\n');
            prompt.push('\n');
            prompt.push_str(&skills_section);
            prompt.push('\n');
        }
    }

    prompt
}
