use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillMetadata {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) instructions_path: PathBuf,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SkillCatalog {
    entries: Vec<SkillMetadata>,
}

#[derive(Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    license: Option<String>,
    compatibility: Option<String>,
    metadata: Option<BTreeMap<String, String>>,
    #[serde(rename = "allowed-tools")]
    allowed_tools: Option<String>,
}

impl SkillCatalog {
    pub(crate) fn discover(global_skills: Option<&Path>, project_root: &Path) -> Self {
        let mut entries = BTreeMap::new();

        if let Some(global_skills) = global_skills {
            Self::insert_source(&mut entries, global_skills);
        }
        Self::insert_source(&mut entries, &project_root.join(".agents/skills"));

        Self {
            entries: entries.into_values().collect(),
        }
    }

    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[SkillMetadata] {
        &self.entries
    }

    pub(crate) fn prompt_section(&self) -> Option<String> {
        (!self.entries.is_empty()).then(|| {
            let mut prompt = String::from(
                "Available skills:\nThese entries are metadata only. When a task matches a skill description, use the read tool to load that skill's full SKILL.md before following its instructions.\n",
            );
            for skill in &self.entries {
                prompt.push_str(&format!(
                    "- {}: {}\n  SKILL.md (literal path): {:?}\n",
                    skill.name, skill.description, skill.instructions_path
                ));
            }
            prompt.trim_end().to_owned()
        })
    }

    fn insert_source(entries: &mut BTreeMap<String, SkillMetadata>, source: &Path) {
        let candidates = fs::read_dir(source)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(|file_type| file_type.is_dir())
                    .map(|_| entry.path())
            })
            .filter_map(|skill_directory| discover_skill(&skill_directory));

        entries.extend(candidates.map(|skill| (skill.name.clone(), skill)));
    }
}

fn discover_skill(skill_directory: &Path) -> Option<SkillMetadata> {
    let instructions_path = skill_directory.join("SKILL.md");
    let contents = fs::read_to_string(&instructions_path).ok()?;
    let SkillFrontmatter {
        name,
        description,
        license,
        compatibility,
        metadata,
        allowed_tools,
    } = parse_frontmatter(&contents)?;

    let _ = (license, metadata, allowed_tools);

    if !is_valid_name(&name)
        || skill_directory.file_name()?.to_str()? != name
        || !(1..=1024).contains(&description.chars().count())
        || compatibility.is_some_and(|value| value.is_empty() || value.chars().count() > 500)
    {
        return None;
    }

    Some(SkillMetadata {
        name,
        description,
        instructions_path: fs::canonicalize(instructions_path).ok()?,
    })
}

fn parse_frontmatter(contents: &str) -> Option<SkillFrontmatter> {
    let mut lines = contents.lines();
    (lines.next()? == "---").then_some(())?;

    let mut yaml = String::new();
    for line in lines {
        if line == "---" {
            let value = yaml_serde::from_str::<yaml_serde::Value>(&yaml).ok()?;
            return has_valid_field_types(&value)
                .then(|| yaml_serde::from_value(value).ok())
                .flatten();
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    None
}

fn has_valid_field_types(value: &yaml_serde::Value) -> bool {
    let yaml_serde::Value::Mapping(fields) = value else {
        return false;
    };

    matches!(fields.get("name"), Some(yaml_serde::Value::String(_)))
        && matches!(
            fields.get("description"),
            Some(yaml_serde::Value::String(_))
        )
        && ["license", "compatibility", "allowed-tools"]
            .into_iter()
            .all(|field| {
                fields
                    .get(field)
                    .is_none_or(|value| matches!(value, yaml_serde::Value::String(_)))
            })
        && fields
            .get("metadata")
            .is_none_or(|metadata| match metadata {
                yaml_serde::Value::Mapping(metadata) => metadata.iter().all(|(key, value)| {
                    matches!(key, yaml_serde::Value::String(_))
                        && matches!(value, yaml_serde::Value::String(_))
                }),
                _ => false,
            })
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::SkillCatalog;
    use std::path::Path;
    use tempfile::tempdir;

    fn write_skill(source: &Path, directory_name: &str, frontmatter: &str, body: &str) {
        let skill = source.join(directory_name);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\n{frontmatter}\n---\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn project_skills_replace_global_skills_and_output_is_sorted() {
        let directory = tempdir().unwrap();
        let global = directory.path().join("global");
        let project = directory.path().join("project");
        write_skill(
            &global,
            "pdf",
            "name: pdf\ndescription: Global PDF help",
            "global body",
        );
        write_skill(
            &global,
            "code-review",
            "name: code-review\ndescription: Review code",
            "review body",
        );
        write_skill(
            &project.join(".agents/skills"),
            "pdf",
            "name: pdf\ndescription: Project PDF help",
            "project body",
        );

        let catalog = SkillCatalog::discover(Some(&global), &project);
        let names: Vec<_> = catalog
            .entries()
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();

        assert_eq!(names, ["code-review", "pdf"]);
        assert_eq!(catalog.entries()[1].description, "Project PDF help");
        assert!(catalog.entries()[1].instructions_path.is_absolute());
        assert!(catalog.prompt_section().unwrap().contains("SKILL.md"));
        assert!(!catalog.prompt_section().unwrap().contains("project body"));
    }

    #[test]
    fn ignores_invalid_candidates_and_nested_resources() {
        let directory = tempdir().unwrap();
        let global = directory.path().join("global");
        let project = directory.path().join("project");
        write_skill(
            &global,
            "valid",
            "name: valid\ndescription: Valid skill\nlicense: MIT\ncompatibility: Moh\nmetadata:\n  owner: platform\nallowed-tools: read",
            "body",
        );
        write_skill(
            &global,
            "mismatch",
            "name: other\ndescription: Wrong directory",
            "body",
        );
        write_skill(
            &global,
            "Upper",
            "name: Upper\ndescription: Uppercase",
            "body",
        );
        write_skill(&global, "empty", "name: empty\ndescription: ''", "body");
        write_skill(
            &global,
            "long-description",
            &format!("name: long-description\ndescription: {}", "a".repeat(1025)),
            "body",
        );
        let overlong_name = "a".repeat(65);
        write_skill(
            &global,
            &overlong_name,
            &format!("name: {overlong_name}\ndescription: Overlong name"),
            "body",
        );
        write_skill(
            &global,
            "empty-compat",
            "name: empty-compat\ndescription: Valid\ncompatibility: ''",
            "body",
        );
        write_skill(
            &global,
            "long-compat",
            &format!(
                "name: long-compat\ndescription: Valid\ncompatibility: {}",
                "a".repeat(501)
            ),
            "body",
        );
        write_skill(
            &global,
            "bad-license",
            "name: bad-license\ndescription: Valid\nlicense: 42",
            "body",
        );
        write_skill(
            &global,
            "bad-tools",
            "name: bad-tools\ndescription: Valid\nallowed-tools: [read]",
            "body",
        );
        write_skill(
            &global,
            "bad-metadata",
            "name: bad-metadata\ndescription: Valid\nmetadata:\n  owner: 42",
            "body",
        );
        write_skill(
            &global,
            "malformed",
            "name: malformed\ndescription: [",
            "body",
        );
        std::fs::create_dir_all(global.join("missing")).unwrap();
        write_skill(
            &global.join("valid/references"),
            "nested",
            "name: nested\ndescription: Nested resource",
            "body",
        );

        let catalog = SkillCatalog::discover(Some(&global), &project);
        let names: Vec<_> = catalog
            .entries()
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();

        assert_eq!(names, ["valid"]);
    }

    #[test]
    fn invalid_project_candidate_does_not_mask_valid_global_candidate() {
        let directory = tempdir().unwrap();
        let global = directory.path().join("global");
        let project = directory.path().join("project");
        write_skill(
            &global,
            "pdf",
            "name: pdf\ndescription: Global PDF help",
            "global body",
        );
        write_skill(
            &project.join(".agents/skills"),
            "pdf",
            "name: pdf\ndescription: [not a string]",
            "project body",
        );

        let catalog = SkillCatalog::discover(Some(&global), &project);

        assert_eq!(catalog.entries().len(), 1);
        assert_eq!(catalog.entries()[0].description, "Global PDF help");
    }

    #[test]
    fn accepts_crlf_frontmatter_and_rejects_missing_delimiters() {
        let directory = tempdir().unwrap();
        let global = directory.path().join("global");
        let crlf = global.join("crlf");
        std::fs::create_dir_all(&crlf).unwrap();
        std::fs::write(
            crlf.join("SKILL.md"),
            "---\r\nname: crlf\r\ndescription: CRLF skill\r\n---\r\nbody: [\r\n",
        )
        .unwrap();
        let unterminated = global.join("unterminated");
        std::fs::create_dir_all(&unterminated).unwrap();
        std::fs::write(
            unterminated.join("SKILL.md"),
            "---\nname: unterminated\ndescription: Missing delimiter\nbody\n",
        )
        .unwrap();
        let no_frontmatter = global.join("no-frontmatter");
        std::fs::create_dir_all(&no_frontmatter).unwrap();
        std::fs::write(no_frontmatter.join("SKILL.md"), "name: no-frontmatter\n").unwrap();

        let catalog = SkillCatalog::discover(Some(&global), directory.path());
        let names: Vec<_> = catalog
            .entries()
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();

        assert_eq!(names, ["crlf"]);
    }

    #[test]
    fn empty_catalog_has_no_prompt_and_inventory_renders_every_path() {
        let directory = tempdir().unwrap();
        let empty = SkillCatalog::discover(None, directory.path());
        assert_eq!(empty.prompt_section(), None);

        let global = directory.path().join("global");
        write_skill(
            &global,
            "pdf",
            "name: pdf\ndescription: PDF help",
            "private body",
        );
        let catalog = SkillCatalog::discover(Some(&global), directory.path());
        let prompt = catalog.prompt_section().unwrap();

        assert!(prompt.contains("use the read tool"));
        assert!(prompt.contains("before following its instructions"));
        for skill in catalog.entries() {
            assert!(prompt.contains(&skill.instructions_path.display().to_string()));
        }
        assert!(!prompt.contains("private body"));
    }
}
