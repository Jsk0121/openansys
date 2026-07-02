use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SkillMetadata {
    pub(crate) dir_path: PathBuf,
    pub(crate) path_to_skills_md: PathBuf,
    pub(crate) name: String,
    pub(crate) description: String,
}

pub(crate) fn discover_skills(
    workspace_root: &Path,
    external_roots: &[PathBuf],
) -> Result<Vec<SkillMetadata>> {
    let mut roots = default_skill_roots(workspace_root);
    roots.extend(external_roots.iter().cloned());

    let mut skills = Vec::new();
    for root in roots {
        let canonical_root = match canonicalize_existing_dir(&root)? {
            Some(path) => path,
            None => continue,
        };

        if canonical_root.join("SKILL.md").is_file() {
            if let Some(skill) = load_skill_from_dir(&canonical_root)? {
                skills.push(skill);
            }
            continue;
        }

        let entries = match fs::read_dir(&canonical_root) {
            Ok(entries) => entries,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to read skills directory {}",
                        canonical_root.display()
                    )
                });
            }
        };

        for entry in entries {
            let path = entry?.path();
            let Some(path) = canonicalize_existing_dir(&path)? else {
                continue;
            };
            if let Some(skill) = load_skill_from_dir(&path)? {
                skills.push(skill);
            }
        }
    }

    skills.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.dir_path.cmp(&right.dir_path))
    });
    skills.dedup_by(|left, right| left.dir_path == right.dir_path);
    Ok(skills)
}

pub(crate) fn render_skills_section(skills: &[SkillMetadata]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }

    let mut rendered = Vec::new();
    rendered.push("## Skills".to_string());
    rendered.push("A skill is a set of local instructions to follow that is stored in a `SKILL.md` file. Below is the list of skills that can be used. Each entry includes a name, description, and file path so you can open the source for full instructions when using a specific skill.".to_string());
    rendered.push("### Available Skills".to_string());
    for skill in skills {
        let path_str = skill.path_to_skills_md.to_string_lossy().replace('\\', "/");
        let name = skill.name.as_str();
        let description = skill.description.as_str();
        rendered.push(format!("- {name}: {description} (file: {path_str})"));
    }
    rendered.push("### How To Use Skills".to_string());
    rendered.push(
        r###"- Discovery: The list above contains the skills available in this session.
- Trigger rules: If the user names a skill or the task clearly matches a skill description, use that skill for the turn.
- Missing or blocked skills: If a named skill is unavailable or unreadable, explain briefly and continue with the best fallback.
- Progressive disclosure: After choosing a skill, read its `SKILL.md`, resolve relative paths from that directory, and load only the files required for the task.
- Coordination: If multiple skills apply, choose the smallest set that covers the task and state the order.
- Safety and fallback: If a skill cannot be applied cleanly, state the issue, choose the next-best approach, and continue."###
            .to_string(),
    );
    return Some(rendered.join("\n"));

    #[allow(unreachable_code)]
    let mut lines: Vec<String> = Vec::new();
    lines.push("## 技能".to_string());
    lines.push("A skill is a set of local instructions to follow that is stored in a `SKILL.md` file. Below is the list of skills that can be used. Each entry includes a name, description, and file path so you can open the source for full instructions when using a specific skill.".to_string());
    lines.push("### 可用技能".to_string());

    for skill in skills {
        let path_str = skill.path_to_skills_md.to_string_lossy().replace('\\', "/");
        let name = skill.name.as_str();
        let description = skill.description.as_str();
        lines.push(format!("- {name}: {description} (file: {path_str})"));
    }

    lines.push("### 如何使用技能".to_string());
    lines.push(
                r###"- 发现：以上列表是本会话中可用的技能（名称 + 描述 + 文件路径）。技能主体存储在列出的路径中。
- 触发规则：如果用户提到技能名称（用 `$SkillName` 或纯文本）或任务明显匹配上述技能描述，你必须在该轮使用该技能。多次提及意味着全部使用。除非再次提及，否则不要跨轮保留技能。
- 缺失/受阻：如果指定的技能不在列表中或路径无法读取，简要说明并继续使用最佳替代方案。
- 如何使用技能（渐进式展开）：
  1) 决定使用某个技能后，使用 shell_tool 读取其 `SKILL.md`。只读取足够遵循工作流程的内容。
  2) 当 `SKILL.md` 引用相对路径（如 `scripts/foo.py`）时，首先相对于上面列出的技能目录解析，仅在需要时考虑其他路径。
  3) 如果 `SKILL.md` 指向额外文件夹如 `references/`，只加载请求所需的特定文件；不要批量加载所有内容。
  4) 如果存在 `scripts/`，优先运行或修补它们，而不是重新输入大量代码块。
  5) 如果存在 `assets/` 或模板，重用它们而不是从头创建。
- 协调与顺序：
  - 如果多个技能适用，选择覆盖请求的最小集合并说明使用顺序。
  - 宣布你正在使用哪些技能以及为什么（一行简短说明）。如果跳过某个明显的技能，说明原因。
- 上下文卫生：
  - 保持上下文精简：总结长段落而不是粘贴它们；仅在需要时加载额外文件。
  - 避免深度引用追踪：除非受阻，优先只打开 `SKILL.md` 直接链接的文件。
  - 当存在变体（框架、提供商、领域）时，只选择相关的参考文件并注明该选择。
- 安全与回退：如果技能无法干净应用（文件缺失、指令不清晰），说明问题，选择次优方案并继续。"###            .to_string(),
    );

    Some(lines.join("\n"))
}

pub(crate) fn parse_external_skill_roots(value: &str) -> Vec<PathBuf> {
    env::split_paths(value)
        .filter(|path| !path.as_os_str().is_empty())
        .collect()
}

fn default_skill_roots(workspace_root: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        workspace_root.join(".mini-codex/skills"),
        workspace_root.join(".agents/skills"),
    ];

    if let Some(home) = home_dir() {
        roots.push(home.join(".mini-codex/skills"));
        roots.push(home.join(".agents/skills"));
    }

    roots
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn canonicalize_existing_dir(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    if !path.is_dir() {
        return Ok(None);
    }
    path.canonicalize()
        .map(Some)
        .with_context(|| format!("failed to resolve {}", path.display()))
}

fn load_skill_from_dir(path: &Path) -> Result<Option<SkillMetadata>> {
    let skill_md = path.join("SKILL.md");
    if !skill_md.is_file() {
        return Ok(None);
    }

    let text = fs::read_to_string(&skill_md)
        .with_context(|| format!("failed to read {}", skill_md.display()))?;
    let Some((name, description)) = parse_skill_frontmatter(&text) else {
        return Ok(None);
    };

    Ok(Some(SkillMetadata {
        dir_path: path.to_path_buf(),
        path_to_skills_md: skill_md,
        name,
        description,
    }))
}

fn parse_skill_frontmatter(text: &str) -> Option<(String, String)> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }

    let mut name = None;
    let mut description = None;

    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("name:") {
            let parsed = trim_yaml_scalar(value);
            if !parsed.is_empty() {
                name = Some(parsed);
            }
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("description:") {
            let parsed = trim_yaml_scalar(value);
            if !parsed.is_empty() {
                description = Some(parsed);
            }
        }
    }

    match (name, description) {
        (Some(name), Some(description)) => Some((name, description)),
        _ => None,
    }
}

fn trim_yaml_scalar(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{parse_external_skill_roots, parse_skill_frontmatter, render_skills_section};
    use std::path::PathBuf;

    #[test]
    fn parses_skill_frontmatter() {
        let text = "\
---
name: pdf-processing
description: Extract PDF text, fill forms, merge files.
---

# PDF Processing
";

        let parsed = parse_skill_frontmatter(text);
        assert_eq!(
            parsed,
            Some((
                "pdf-processing".to_string(),
                "Extract PDF text, fill forms, merge files.".to_string()
            ))
        );
    }

    #[test]
    fn renders_skills_section() {
        let rendered = render_skills_section(&[super::SkillMetadata {
            dir_path: PathBuf::from("/tmp/pdf-processing"),
            path_to_skills_md: PathBuf::from("/tmp/pdf-processing/SKILL.md"),
            name: "pdf-processing".to_string(),
            description: "Extract PDF text.".to_string(),
        }])
        .expect("skills section");

        assert!(rendered.contains("## Skills"));
        assert!(
            rendered.contains(
                "- pdf-processing: Extract PDF text. (file: /tmp/pdf-processing/SKILL.md)"
            )
        );
    }

    #[test]
    fn parses_external_skill_roots_with_platform_separator() {
        let joined = std::env::join_paths([
            PathBuf::from("/tmp/skills-a"),
            PathBuf::from("/tmp/skills-b"),
        ])
        .expect("joined paths");

        let parsed = parse_external_skill_roots(joined.to_str().expect("utf-8"));
        assert_eq!(
            parsed,
            vec![
                PathBuf::from("/tmp/skills-a"),
                PathBuf::from("/tmp/skills-b")
            ]
        );
    }
}
