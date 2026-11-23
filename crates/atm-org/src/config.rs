//! Pure config parsing. Roles support built-in harnesses only.

use crate::{ids::validate_name, IdError, ProjectId};
use atm_core::{find_harness_definition, Harness};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Id(#[from] IdError),
    #[error("role {name:?}: missing opening +++ front matter delimiter")]
    MissingOpening { name: String },
    #[error("role {name:?}: unterminated +++ front matter")]
    Unterminated { name: String },
    #[error("{context}: TOML: {source}")]
    Toml {
        context: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("role {name:?}: {problem}")]
    Role { name: String, problem: String },
    #[error("cannot encode project TOML: {0}")]
    Encode(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectFile {
    pub id: ProjectId,
}
impl ProjectFile {
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string(self)?)
    }
}
pub fn parse_project(text: &str) -> Result<ProjectFile, ConfigError> {
    toml::from_str(text).map_err(|source| ConfigError::Toml {
        context: "project".into(),
        source,
    })
}
#[must_use]
pub fn new_project_file(dir_name: &str) -> ProjectFile {
    ProjectFile {
        id: ProjectId::from_folder(dir_name),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleSpec {
    pub name: String,
    #[serde(flatten)]
    pub config: RoleConfig,
    pub instructions: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleConfig {
    pub harness: Harness,
    pub model: Option<String>,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    pub manager: Option<String>,
    pub profile: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub spawn_args: Vec<String>,
    #[serde(default)]
    pub command_prefix: Vec<String>,
    pub pane: Option<String>,
    pub clear: Option<String>,
    #[serde(default)]
    pub trust_dirs: Vec<String>,
    /// The reasons the daemon may hand this seat over without a command;
    /// empty is off. Read through [`crate::auto_handover::handover_settings`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_handover: Vec<String>,
    /// The seat's context line in percent, when it is not the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_line: Option<u8>,
    /// The prompt cache life the role's author assumes. No built-in value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_minutes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_minutes: Option<u32>,
    /// The role groups this seat answers to (`@builders`), each a name in the
    /// id grammar. Empty is not serialized, so role snapshots keep their bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
}
fn default_cwd() -> String {
    ".".into()
}

pub fn parse_role(name: &str, text: &str) -> Result<RoleSpec, ConfigError> {
    validate_name(name)?;
    let mut lines = text.split_inclusive('\n');
    let opening = lines
        .next()
        .filter(|line| line.trim_end_matches(['\r', '\n']) == "+++")
        .ok_or_else(|| ConfigError::MissingOpening { name: name.into() })?;
    let start = opening.len();
    let mut end = start;
    for line in lines {
        if line.trim_end_matches(['\r', '\n']) == "+++" {
            let header = text.get(start..end).unwrap_or_default();
            let config = parse_front_matter(name, header)?;
            return Ok(RoleSpec {
                name: name.into(),
                config,
                instructions: text.get(end + line.len()..).unwrap_or_default().into(),
            });
        }
        end += line.len();
    }
    Err(ConfigError::Unterminated { name: name.into() })
}

fn parse_front_matter(name: &str, text: &str) -> Result<RoleConfig, ConfigError> {
    let toml_error = |source| ConfigError::Toml {
        context: format!("role {name:?}"),
        source,
    };
    let table: toml::Table = toml::from_str(text).map_err(toml_error)?;
    // This boundary is strict, but persisted event JSON is tolerant of new fields.
    const KEYS: &[&str] = &[
        "harness",
        "model",
        "cwd",
        "manager",
        "profile",
        "skills",
        "spawn_args",
        "command_prefix",
        "pane",
        "clear",
        "trust_dirs",
        "auto_handover",
        "context_line",
        "cache_minutes",
        "idle_minutes",
        "groups",
    ];
    for key in table.keys() {
        if !KEYS.contains(&key.as_str()) {
            return Err(ConfigError::Role {
                name: name.into(),
                problem: format!("unknown key {key:?}"),
            });
        }
    }
    let harness = table
        .get("harness")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| ConfigError::Role {
            name: name.into(),
            problem: "missing or non-string harness".into(),
        })?;
    let definition = find_harness_definition(harness).ok_or_else(|| ConfigError::Role {
        name: name.into(),
        problem: format!("unknown built-in harness {harness:?}"),
    })?;
    let mut config: RoleConfig = table.try_into().map_err(toml_error)?;
    config.harness = definition.harness;
    if let Some(bad) = config.groups.iter().find(|g| validate_name(g).is_err()) {
        return Err(ConfigError::Role {
            name: name.into(),
            problem: format!("invalid group name {bad:?}"),
        });
    }
    crate::auto_handover::handover_settings(&config).map_err(|problem| ConfigError::Role {
        name: name.into(),
        problem: problem.to_string(),
    })?;
    Ok(config)
}
