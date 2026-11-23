//! Validated textual organisation identities.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid {kind}: {value:?} ({rule})")]
pub struct IdError {
    pub kind: &'static str,
    pub value: String,
    pub rule: &'static str,
}

fn invalid(kind: &'static str, value: &str, rule: &'static str) -> IdError {
    IdError {
        kind,
        value: value.into(),
        rule,
    }
}

macro_rules! text_serde {
    ($ty:ty) => {
        impl TryFrom<String> for $ty {
            type Error = IdError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                value.parse()
            }
        }
        impl From<$ty> for String {
            fn from(value: $ty) -> Self {
                value.to_string()
            }
        }
    };
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectId(String);
text_serde!(ProjectId);

pub(crate) fn slug_chars(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('-')
        && text.len() <= 32
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl ProjectId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `index` is the zero-based collision number, producing suffixes 2, 3, ...
    pub(crate) fn suffixed(&self, index: usize) -> Self {
        // usize is at most 64 bits on supported targets: widening before adding
        // leaves room for len + 1, and the suffix takes at most 21 ASCII bytes.
        let suffix = format!("-{}", (index as u128).saturating_add(2));
        let prefix = self
            .0
            .get(..32_usize.saturating_sub(suffix.len()))
            .unwrap_or(&self.0);
        Self(format!("{}{suffix}", prefix.trim_end_matches('-')))
    }

    pub(crate) fn from_folder(text: &str) -> Self {
        let mut slug = String::new();
        for c in text.chars() {
            if c.is_ascii_alphanumeric() {
                slug.push(c.to_ascii_lowercase());
            } else if !slug.is_empty() && !slug.ends_with('-') {
                slug.push('-');
            }
        }
        slug.truncate(32); // ASCII only, so every byte is a character boundary.
        let slug = slug.trim_end_matches('-');
        Self(if slug.is_empty() { "project" } else { slug }.into())
    }
}
impl FromStr for ProjectId {
    type Err = IdError;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if slug_chars(text) {
            Ok(Self(text.into()))
        } else {
            Err(invalid(
                "project id",
                text,
                "expected [a-z0-9-]{1,32}, starting with a letter or digit",
            ))
        }
    }
}
impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub(crate) fn validate_name(name: &str) -> Result<(), IdError> {
    if slug_chars(name) && !matches!(name, "owner" | "system") {
        Ok(())
    } else {
        Err(invalid("agent name", name, "expected 1..32 lowercase ASCII letters, digits or hyphens; first character alphanumeric; owner/system reserved"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentId {
    pub project: ProjectId,
    pub name: String,
}
text_serde!(AgentId);
impl AgentId {
    pub fn new(project: ProjectId, name: impl Into<String>) -> Result<Self, IdError> {
        let id = Self {
            project,
            name: name.into(),
        };
        id.validate()?;
        Ok(id)
    }
    pub fn validate(&self) -> Result<(), IdError> {
        validate_name(&self.name)
    }
}
impl FromStr for AgentId {
    type Err = IdError;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (project, name) = text
            .split_once('/')
            .ok_or_else(|| invalid("agent id", text, "expected project/name"))?;
        Self::new(project.parse()?, name)
    }
}
impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.project, self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MessageId(u64);
text_serde!(MessageId);
impl MessageId {
    #[must_use]
    pub fn new(seq: u64) -> Self {
        Self(seq)
    }
    #[must_use]
    pub fn seq(self) -> u64 {
        self.0
    }
}
impl FromStr for MessageId {
    type Err = IdError;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let value = text
            .strip_prefix('m')
            .and_then(|s| s.parse::<u64>().ok())
            .map(Self);
        value.filter(|id| id.to_string() == text).ok_or_else(|| {
            invalid(
                "message id",
                text,
                "expected m followed by canonical u64 decimal",
            )
        })
    }
}
impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "m{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TaskId(String);
text_serde!(TaskId);
impl FromStr for TaskId {
    type Err = IdError;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty() {
            Err(invalid("task id", text, "must not be empty"))
        } else {
            Ok(Self(text.into()))
        }
    }
}
impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Address {
    Agent(AgentId),
    Owner,
    System,
}
text_serde!(Address);
impl FromStr for Address {
    type Err = IdError;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "owner" => Ok(Self::Owner),
            "system" => Ok(Self::System),
            _ => text.parse().map(Self::Agent),
        }
    }
}
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(id) => id.fmt(f),
            Self::Owner => f.write_str("owner"),
            Self::System => f.write_str("system"),
        }
    }
}

/// Where a post lives. Text: a project id, or two distinct addresses joined by
/// `+` in ascending [`Address`] order (the derived order: agents by project
/// then name, then `owner`, then `system`; not text order), such as
/// `p/lead+owner`. Any other text is [`ChannelId::Other`], a later form this
/// version folds but never writes. Every text round-trips exactly.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum ChannelId {
    Project(ProjectId),
    Direct(Address, Address),
    /// Read-only: neither a project id nor a canonical pair.
    Other(UnknownChannel),
}

/// The text of a channel id this version does not know. Only decoding makes one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnknownChannel(String);
impl UnknownChannel {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ChannelId {
    /// The direct channel of two distinct addresses, given in either order.
    #[must_use]
    pub fn direct(a: Address, b: Address) -> Option<Self> {
        match a.cmp(&b) {
            std::cmp::Ordering::Less => Some(Self::Direct(a, b)),
            std::cmp::Ordering::Greater => Some(Self::Direct(b, a)),
            std::cmp::Ordering::Equal => None,
        }
    }

    /// False for a later form, and for a pair built out of order, which
    /// commands refuse.
    #[must_use]
    pub fn is_known(&self) -> bool {
        match self {
            Self::Project(_) => true,
            Self::Direct(a, b) => a < b,
            Self::Other(_) => false,
        }
    }
}
impl From<String> for ChannelId {
    fn from(text: String) -> Self {
        if let Ok(project) = text.parse() {
            return Self::Project(project);
        }
        let pair = text
            .split_once('+')
            .and_then(|(a, b)| Some((a.parse::<Address>().ok()?, b.parse::<Address>().ok()?)));
        match pair {
            // Address text is canonical, so an ordered pair prints as `text`.
            Some((a, b)) if a < b => Self::Direct(a, b),
            _ => Self::Other(UnknownChannel(text)),
        }
    }
}
impl From<ChannelId> for String {
    fn from(value: ChannelId) -> Self {
        value.to_string()
    }
}
impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Project(id) => id.fmt(f),
            Self::Direct(a, b) => write!(f, "{a}+{b}"),
            Self::Other(text) => f.write_str(&text.0),
        }
    }
}
