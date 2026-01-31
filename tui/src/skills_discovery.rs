use serde::Deserialize;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;

const SKILL_FILENAME: &str = "SKILL.md";
const SKILLS_DIR_NAME: &str = "skills";
const SYSTEM_SKILLS_DIR_NAME: &str = ".system";
const MAX_SCAN_DEPTH: usize = 6;
const MAX_SKILLS_DIRS_PER_ROOT: usize = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SkillScope {
    Repo,
    User,
    System,
    Admin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
    pub interface: Option<SkillInterface>,
    pub path: PathBuf,
    pub scope: SkillScope,
}

impl SkillMetadata {
    pub fn display_name(&self) -> &str {
        self.interface
            .as_ref()
            .and_then(|interface| interface.display_name.as_deref())
            .unwrap_or(&self.name)
    }

    pub fn display_description(&self) -> &str {
        self.interface
            .as_ref()
            .and_then(|interface| interface.short_description.as_deref())
            .or(self.short_description.as_deref())
            .unwrap_or(&self.description)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SkillInterface {
    pub display_name: Option<String>,
    pub short_description: Option<String>,
}

pub fn load_skills(cwd: &Path) -> Vec<SkillMetadata> {
    let roots = skill_roots(cwd);

    let mut out = Vec::new();
    for root in roots {
        discover_skills_under_root(&root, &mut out);
    }

    let mut seen_paths = HashSet::<PathBuf>::new();
    out.retain(|skill| seen_paths.insert(skill.path.clone()));

    fn scope_rank(scope: SkillScope) -> u8 {
        match scope {
            SkillScope::Repo => 0,
            SkillScope::User => 1,
            SkillScope::System => 2,
            SkillScope::Admin => 3,
        }
    }

    out.sort_by(|a, b| {
        scope_rank(a.scope)
            .cmp(&scope_rank(b.scope))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path.cmp(&b.path))
    });

    out
}

#[derive(Clone, Debug)]
struct SkillRoot {
    path: PathBuf,
    scope: SkillScope,
    follow_symlinks: bool,
}

fn skill_roots(cwd: &Path) -> Vec<SkillRoot> {
    let mut roots = Vec::new();

    for dir in repo_dirs_between(cwd) {
        let skills_dir = dir.join(".codex").join(SKILLS_DIR_NAME);
        if skills_dir.is_dir() {
            roots.push(SkillRoot {
                path: skills_dir,
                scope: SkillScope::Repo,
                follow_symlinks: true,
            });
        }
    }

    if let Some(codex_home) = find_codex_home() {
        let user_skills = codex_home.join(SKILLS_DIR_NAME);
        roots.push(SkillRoot {
            path: user_skills.join(SYSTEM_SKILLS_DIR_NAME),
            scope: SkillScope::System,
            follow_symlinks: false,
        });
        roots.push(SkillRoot {
            path: user_skills,
            scope: SkillScope::User,
            follow_symlinks: true,
        });
    }

    #[cfg(unix)]
    {
        roots.push(SkillRoot {
            path: PathBuf::from("/etc/codex").join(SKILLS_DIR_NAME),
            scope: SkillScope::Admin,
            follow_symlinks: true,
        });
    }

    roots
}

fn repo_dirs_between(cwd: &Path) -> Vec<&Path> {
    let repo_root = find_repo_root(cwd);

    // Highest precedence first (closest to cwd).
    cwd.ancestors()
        .scan(false, |done, ancestor| {
            if *done {
                None
            } else {
                if Some(ancestor) == repo_root.as_deref() {
                    *done = true;
                }
                Some(ancestor)
            }
        })
        .collect::<Vec<_>>()
}

fn find_repo_root(cwd: &Path) -> Option<PathBuf> {
    for ancestor in cwd.ancestors() {
        if ancestor.join(".git").exists() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

fn find_codex_home() -> Option<PathBuf> {
    if let Ok(val) = std::env::var("CODEX_HOME")
        && !val.is_empty()
    {
        return Some(PathBuf::from(val));
    }
    dirs::home_dir().map(|home| home.join(".codex"))
}

fn discover_skills_under_root(root: &SkillRoot, out: &mut Vec<SkillMetadata>) {
    let Ok(root_dir) = std::fs::canonicalize(&root.path) else {
        return;
    };
    if !root_dir.is_dir() {
        return;
    }

    fn enqueue_dir(
        queue: &mut VecDeque<(PathBuf, usize)>,
        visited: &mut HashSet<PathBuf>,
        truncated_by_dir_limit: &mut bool,
        path: PathBuf,
        depth: usize,
    ) {
        if depth > MAX_SCAN_DEPTH {
            return;
        }
        if visited.len() >= MAX_SKILLS_DIRS_PER_ROOT {
            *truncated_by_dir_limit = true;
            return;
        }
        if visited.insert(path.clone()) {
            queue.push_back((path, depth));
        }
    }

    let mut visited_dirs = HashSet::<PathBuf>::new();
    visited_dirs.insert(root_dir.clone());

    let mut queue = VecDeque::from([(root_dir.clone(), 0)]);
    let mut truncated_by_dir_limit = false;

    while let Some((dir, depth)) = queue.pop_front() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::warn!("failed to read skills dir {}: {err}", dir.display());
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = match path.file_name().and_then(|name| name.to_str()) {
                Some(name) => name,
                None => continue,
            };

            if file_name.starts_with('.') {
                continue;
            }

            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };

            if file_type.is_symlink() {
                if !root.follow_symlinks {
                    continue;
                }

                let metadata = match std::fs::metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(err) => {
                        tracing::warn!(
                            "failed to stat skills entry {} (symlink): {err}",
                            path.display()
                        );
                        continue;
                    }
                };

                if metadata.is_dir() {
                    let Ok(resolved_dir) = std::fs::canonicalize(&path) else {
                        continue;
                    };
                    enqueue_dir(
                        &mut queue,
                        &mut visited_dirs,
                        &mut truncated_by_dir_limit,
                        resolved_dir,
                        depth + 1,
                    );
                }

                continue;
            }

            if file_type.is_dir() {
                let Ok(resolved_dir) = std::fs::canonicalize(&path) else {
                    continue;
                };
                enqueue_dir(
                    &mut queue,
                    &mut visited_dirs,
                    &mut truncated_by_dir_limit,
                    resolved_dir,
                    depth + 1,
                );
                continue;
            }

            if file_type.is_file() && file_name == SKILL_FILENAME {
                match parse_skill_file(&path, root.scope) {
                    Ok(skill) => out.push(skill),
                    Err(err) => {
                        tracing::warn!("failed to parse {}: {err}", path.display());
                    }
                }
            }
        }
    }

    if truncated_by_dir_limit {
        tracing::warn!(
            "skills scan truncated after {MAX_SKILLS_DIRS_PER_ROOT} directories (root: {})",
            root_dir.display()
        );
    }
}

#[derive(Debug)]
enum SkillParseError {
    Read(std::io::Error),
    MissingFrontmatter,
    InvalidYaml(serde_yaml::Error),
    MissingField(&'static str),
}

impl std::fmt::Display for SkillParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillParseError::Read(err) => write!(f, "failed to read file: {err}"),
            SkillParseError::MissingFrontmatter => {
                write!(f, "missing YAML frontmatter delimited by ---")
            }
            SkillParseError::InvalidYaml(err) => write!(f, "invalid YAML: {err}"),
            SkillParseError::MissingField(field) => write!(f, "missing field `{field}`"),
        }
    }
}

impl std::error::Error for SkillParseError {}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    metadata: SkillFrontmatterMetadata,
}

#[derive(Debug, Default, Deserialize)]
struct SkillFrontmatterMetadata {
    #[serde(default, rename = "short-description")]
    short_description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SkillMetadataFile {
    #[serde(default)]
    interface: Option<SkillInterfaceFile>,
}

#[derive(Debug, Default, Deserialize)]
struct SkillInterfaceFile {
    display_name: Option<String>,
    short_description: Option<String>,
}

fn parse_skill_file(path: &Path, scope: SkillScope) -> Result<SkillMetadata, SkillParseError> {
    let contents = std::fs::read_to_string(path).map_err(SkillParseError::Read)?;
    let frontmatter = extract_frontmatter(&contents).ok_or(SkillParseError::MissingFrontmatter)?;

    let parsed: SkillFrontmatter =
        serde_yaml::from_str(&frontmatter).map_err(SkillParseError::InvalidYaml)?;

    if parsed.name.trim().is_empty() {
        return Err(SkillParseError::MissingField("name"));
    }
    if parsed.description.trim().is_empty() {
        return Err(SkillParseError::MissingField("description"));
    }

    let resolved_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let interface = load_skill_interface(path);

    Ok(SkillMetadata {
        name: parsed.name.trim().to_string(),
        description: parsed.description.trim().to_string(),
        short_description: parsed
            .metadata
            .short_description
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        interface,
        path: resolved_path,
        scope,
    })
}

fn extract_frontmatter(contents: &str) -> Option<String> {
    let mut lines = contents.lines();
    let first = lines.next()?.trim_end_matches('\r');
    if first != "---" {
        return None;
    }

    let mut yaml = String::new();
    for line in lines {
        let trimmed = line.trim_end_matches('\r');
        if trimmed == "---" {
            return Some(yaml);
        }
        yaml.push_str(trimmed);
        yaml.push('\n');
    }

    None
}

fn load_skill_interface(skill_path: &Path) -> Option<SkillInterface> {
    let skill_dir = skill_path.parent()?;

    let metadata_path = skill_dir.join("agents").join("openai.yaml");
    if !metadata_path.exists() {
        return None;
    }

    let contents = match std::fs::read_to_string(&metadata_path) {
        Ok(contents) => contents,
        Err(err) => {
            tracing::warn!(
                "ignoring {}: failed to read openai.yaml: {err}",
                metadata_path.display()
            );
            return None;
        }
    };

    let parsed: SkillMetadataFile = match serde_yaml::from_str(&contents) {
        Ok(parsed) => parsed,
        Err(err) => {
            tracing::warn!(
                "ignoring {}: invalid openai.yaml: {err}",
                metadata_path.display()
            );
            return None;
        }
    };

    let interface = parsed.interface?;
    let display_name = interface
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let short_description = interface
        .short_description
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    if display_name.is_none() && short_description.is_none() {
        None
    } else {
        Some(SkillInterface {
            display_name,
            short_description,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_frontmatter_name_description_and_short_description() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skill_dir = dir.path().join("skills").join("my-skill");
        std::fs::create_dir_all(&skill_dir).expect("mkdir skill");
        let skill_path = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_path,
            r#"---
name: my-skill
description: My test skill.
metadata:
  short-description: Short!
---

# Body
"#,
        )
        .expect("write skill");

        let parsed = parse_skill_file(&skill_path, SkillScope::User).expect("parse");
        assert_eq!(parsed.name, "my-skill");
        assert_eq!(parsed.description, "My test skill.");
        assert_eq!(parsed.short_description.as_deref(), Some("Short!"));
        assert_eq!(parsed.scope, SkillScope::User);
    }
}
