//! System prompt assembly.
//!
//! Follows the shape of pi's `buildSystemPrompt`: a base prompt, the tools that
//! are actually registered, guidelines, the project's own instructions, the
//! skills available, and the working directory. A custom prompt replaces the
//! base and the guidelines but keeps everything the project contributed —
//! overriding the instructions should not also discard the repository's
//! conventions.

use std::path::Path;

use crate::context::ContextFile;
use crate::skills::Skill;

/// The default instructions.
pub const BASE: &str = "\
You are aphid, a coding agent working in a terminal. You have direct access to the \
user's machine through tools and are expected to use them rather than to guess.

Work like a careful colleague: read before you change, make the smallest change that \
does the job, and verify it. When you change code, run whatever the project uses to \
check it. Report what you actually did, including what failed.";

/// What goes into a prompt.
#[derive(Clone, Debug, Default)]
pub struct PromptOptions {
    /// Replaces [`BASE`] and the guidelines.
    pub custom: Option<String>,
    /// Appended after the instructions, before project context.
    pub append: Option<String>,
    /// `(name, one-line summary)` for each registered tool.
    pub tools: Vec<(String, String)>,
    /// Extra bullets added to the guidelines.
    pub guidelines: Vec<String>,
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
}

/// Build the system prompt.
#[must_use]
pub fn build(options: &PromptOptions, cwd: &Path) -> String {
    let mut prompt = String::with_capacity(2048);

    match &options.custom {
        Some(custom) => prompt.push_str(custom.trim_end()),
        None => {
            prompt.push_str(BASE);
            push_tools(&mut prompt, &options.tools);
            push_guidelines(&mut prompt, options);
        }
    }

    if let Some(append) = &options.append {
        let append = append.trim();
        if !append.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(append);
        }
    }

    push_context(&mut prompt, &options.context_files);
    push_skills(&mut prompt, &options.skills);

    prompt.push_str("\n\nCurrent working directory: ");
    prompt.push_str(&cwd.display().to_string().replace('\\', "/"));
    prompt
}

fn push_tools(prompt: &mut String, tools: &[(String, String)]) {
    if tools.is_empty() {
        return;
    }
    prompt.push_str("\n\nAvailable tools:\n");
    for (name, snippet) in tools {
        prompt.push_str(&format!("- {name}: {snippet}\n"));
    }
    prompt.truncate(prompt.trim_end().len());
}

fn push_guidelines(prompt: &mut String, options: &PromptOptions) {
    let has = |name: &str| options.tools.iter().any(|(tool, _)| tool == name);

    let mut guidelines: Vec<String> = Vec::new();
    if has("bash") {
        guidelines
            .push("Use bash for anything the other tools do not cover: ls, rg, find, git".into());
    }
    if has("edit") {
        guidelines.push(
            "Prefer edit over write for existing files; include enough surrounding text that \
             each old_text matches exactly once"
                .into(),
        );
    }
    if has("read") {
        guidelines.push("Read a file before editing it".into());
    }
    guidelines.push("Show file paths clearly when working with files".into());
    guidelines.push("Be concise".into());

    for extra in &options.guidelines {
        let extra = extra.trim();
        if !extra.is_empty() && !guidelines.iter().any(|g| g == extra) {
            guidelines.push(extra.to_owned());
        }
    }

    prompt.push_str("\n\nGuidelines:\n");
    for guideline in &guidelines {
        prompt.push_str(&format!("- {guideline}\n"));
    }
    prompt.truncate(prompt.trim_end().len());
}

fn push_context(prompt: &mut String, files: &[ContextFile]) {
    if files.is_empty() {
        return;
    }
    prompt.push_str("\n\n<project_context>\nProject-specific instructions:\n");
    for file in files {
        prompt.push_str(&format!(
            "\n<project_instructions path=\"{}\">\n{}\n</project_instructions>\n",
            escape(&file.path.display().to_string()),
            file.content.trim_end()
        ));
    }
    prompt.push_str("</project_context>");
}

fn push_skills(prompt: &mut String, skills: &[Skill]) {
    if skills.is_empty() {
        return;
    }
    prompt.push_str(
        "\n\nThe following skills hold detailed instructions for specific tasks. Read the whole \
         skill file with the read tool when a task matches its description. Paths inside a skill \
         are relative to that skill's directory.\n\n<available_skills>\n",
    );
    for skill in skills {
        prompt.push_str(&format!(
            "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    \
             <location>{}</location>\n  </skill>\n",
            escape(&skill.name),
            escape(&skill.description),
            escape(&skill.path.display().to_string()),
        ));
    }
    prompt.push_str("</available_skills>");
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn options() -> PromptOptions {
        PromptOptions {
            tools: vec![
                ("bash".into(), "run a shell command".into()),
                ("edit".into(), "replace exact text".into()),
            ],
            ..PromptOptions::default()
        }
    }

    #[test]
    fn the_default_prompt_lists_tools_guidelines_and_cwd() {
        let prompt = build(&options(), Path::new("/work/project"));

        assert!(prompt.starts_with("You are aphid"));
        assert!(prompt.contains("- bash: run a shell command"));
        assert!(prompt.contains("Prefer edit over write"));
        assert!(prompt.ends_with("Current working directory: /work/project"));
    }

    #[test]
    fn guidelines_follow_the_tools_that_are_actually_registered() {
        let mut options = options();
        options.tools.retain(|(name, _)| name == "bash");
        let prompt = build(&options, Path::new("/w"));

        assert!(prompt.contains("Use bash for anything"));
        assert!(!prompt.contains("Prefer edit over write"));
    }

    #[test]
    fn a_custom_prompt_replaces_the_base_but_keeps_project_context() {
        let mut options = options();
        options.custom = Some("Only do what you are told.".into());
        options.context_files = vec![ContextFile {
            path: PathBuf::from("/w/AGENTS.md"),
            content: "Run cargo fmt.".into(),
        }];

        let prompt = build(&options, Path::new("/w"));

        assert!(prompt.starts_with("Only do what you are told."));
        assert!(!prompt.contains("You are aphid"));
        assert!(!prompt.contains("Guidelines:"));
        assert!(prompt.contains("Run cargo fmt."));
        assert!(prompt.contains(r#"<project_instructions path="/w/AGENTS.md">"#));
    }

    #[test]
    fn skills_are_advertised_but_not_inlined() {
        let mut options = options();
        options.skills = vec![Skill {
            name: "release".into(),
            description: "How to cut a release".into(),
            path: PathBuf::from("/w/.aphid/skills/release/SKILL.md"),
        }];

        let prompt = build(&options, Path::new("/w"));

        assert!(prompt.contains("<name>release</name>"));
        assert!(prompt.contains("<description>How to cut a release</description>"));
        assert!(prompt.contains("<location>/w/.aphid/skills/release/SKILL.md</location>"));
    }

    #[test]
    fn markup_in_values_is_escaped() {
        let mut options = options();
        options.skills = vec![Skill {
            name: "x".into(),
            description: "handles <tags> & \"quotes\"".into(),
            path: PathBuf::from("/w/x.md"),
        }];

        let prompt = build(&options, Path::new("/w"));

        assert!(prompt.contains("handles &lt;tags&gt; &amp; &quot;quotes&quot;"));
    }

    #[test]
    fn append_lands_before_the_project_context() {
        let mut options = options();
        options.append = Some("Extra rule.".into());
        options.context_files = vec![ContextFile {
            path: PathBuf::from("/w/AGENTS.md"),
            content: "Project rule.".into(),
        }];

        let prompt = build(&options, Path::new("/w"));
        let append_at = prompt.find("Extra rule.").expect("append");
        let context_at = prompt.find("Project rule.").expect("context");
        assert!(append_at < context_at);
    }
}
