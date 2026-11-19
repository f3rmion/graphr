use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize, de::IgnoredAny};

pub(crate) const PLAN_LIMIT: u64 = 16 * 1024 * 1024;
const MAX_PLAN_CHANGES: usize = 8192;
const MAX_ADDRESS_BYTES: usize = 512;

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub(crate) complete: Option<bool>,
    pub(crate) applyable: Option<bool>,
    pub(crate) errored: Option<bool>,
    pub(crate) changes: Vec<Change>,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Change {
    pub(crate) address: String,
    pub(crate) previous_address: Option<String>,
    pub(crate) deposed: Option<String>,
    pub(crate) action: Action,
    pub(crate) importing: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Create,
    Update,
    Delete,
    Read,
    DeleteThenCreate,
    CreateThenDelete,
    Forget,
    ForgetThenCreate,
    CreateThenForget,
    Move,
    Import,
    Unsupported,
}

impl Action {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Read => "read",
            Self::DeleteThenCreate | Self::CreateThenDelete => "replace",
            Self::ForgetThenCreate | Self::CreateThenForget => "replace-and-forget",
            Self::Forget => "forget",
            Self::Move => "move",
            Self::Import => "import",
            Self::Unsupported => "unsupported",
        }
    }

    pub(crate) fn order(self) -> Option<&'static str> {
        match self {
            Self::DeleteThenCreate => Some("delete,create"),
            Self::CreateThenDelete => Some("create,delete"),
            Self::ForgetThenCreate => Some("forget,create"),
            Self::CreateThenForget => Some("create,forget"),
            _ => None,
        }
    }

    pub(crate) fn may_use_base(self) -> bool {
        matches!(
            self,
            Self::Delete
                | Self::DeleteThenCreate
                | Self::CreateThenDelete
                | Self::Forget
                | Self::ForgetThenCreate
                | Self::CreateThenForget
        )
    }
}

#[derive(Deserialize)]
struct ShowPlan {
    format_version: String,
    // Present in a plan from terraform show, unlike the state JSON format.
    #[serde(rename = "configuration")]
    _configuration: ShowConfiguration,
    complete: Option<bool>,
    applyable: Option<bool>,
    errored: Option<bool>,
    #[serde(default)]
    resource_changes: Vec<ShowResourceChange>,
}

#[derive(Deserialize)]
struct ShowConfiguration {
    #[serde(rename = "root_module")]
    _root_module: IgnoredAny,
}

#[derive(Deserialize)]
struct ShowResourceChange {
    address: String,
    previous_address: Option<String>,
    deposed: Option<String>,
    change: ShowChange,
}

#[derive(Deserialize)]
struct ShowChange {
    actions: Vec<String>,
    importing: Option<IgnoredAny>,
}

pub(crate) fn decode(bytes: &[u8], cancelled: &AtomicBool) -> Result<Plan, &'static str> {
    check_cancelled(cancelled)?;
    let raw: ShowPlan =
        rmcp::serde_json::from_slice(bytes).map_err(|_| "Terraform plan JSON is invalid")?;
    if !raw
        .format_version
        .strip_prefix("1.")
        .is_some_and(|minor| !minor.is_empty() && minor.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err("Terraform plan JSON format version is unsupported");
    }
    if raw.resource_changes.len() > MAX_PLAN_CHANGES {
        return Err("Terraform plan has too many resource changes");
    }
    let mut changes = Vec::new();
    for entry in raw.resource_changes {
        check_cancelled(cancelled)?;
        let importing = entry.change.importing.is_some();
        let previous_address = entry.previous_address.filter(|prev| *prev != entry.address);
        let action = match entry
            .change
            .actions
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            ["no-op"] if previous_address.is_some() => Action::Move,
            ["no-op"] if importing => Action::Import,
            ["no-op"] => continue,
            ["create"] => Action::Create,
            ["update"] => Action::Update,
            ["delete"] => Action::Delete,
            ["read"] => Action::Read,
            ["delete", "create"] => Action::DeleteThenCreate,
            ["create", "delete"] => Action::CreateThenDelete,
            ["forget"] => Action::Forget,
            ["forget", "create"] => Action::ForgetThenCreate,
            ["create", "forget"] => Action::CreateThenForget,
            [] => return Err("Terraform plan resource change has no actions"),
            _ => Action::Unsupported,
        };
        changes.push(Change {
            address: entry.address,
            previous_address,
            deposed: entry.deposed,
            action,
            importing,
        });
    }
    changes.sort_unstable_by(|left, right| identity(left).cmp(&identity(right)));
    let plan = Plan {
        complete: raw.complete,
        applyable: raw.applyable,
        errored: raw.errored,
        changes,
    };
    plan.validate()?;
    Ok(plan)
}

impl Plan {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.changes.len() > MAX_PLAN_CHANGES {
            return Err("Terraform plan has too many resource changes");
        }
        if self
            .changes
            .windows(2)
            .any(|pair| identity(&pair[0]) >= identity(&pair[1]))
        {
            return Err("Terraform plan resource identities are duplicated or out of order");
        }
        for change in &self.changes {
            if declaration_address(&change.address).is_none()
                || change.previous_address.as_deref().is_some_and(|previous| {
                    previous == change.address || declaration_address(previous).is_none()
                })
            {
                return Err("Terraform plan resource address is invalid");
            }
            if change.deposed.as_deref().is_some_and(|key| {
                key.len() != 8 || !key.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                return Err("Terraform plan deposed key is invalid");
            }
            if (change.action == Action::Move && change.previous_address.is_none())
                || (change.action == Action::Import && !change.importing)
            {
                return Err("Terraform plan action metadata is invalid");
            }
        }
        Ok(())
    }
}

fn identity(change: &Change) -> (&str, Option<&str>) {
    (&change.address, change.deposed.as_deref())
}

pub(crate) fn root_is_safe(root: &str) -> bool {
    root == "." || crate::evidence::evidence_path_is_safe(root)
}

/// Converts an instance address to the declaration address in the static graph.
pub(crate) fn declaration_address(mut address: &str) -> Option<String> {
    if address.len() > MAX_ADDRESS_BYTES || address.chars().any(char::is_control) {
        return None;
    }
    let mut declaration = String::with_capacity(address.len());
    while let Some(rest) = address.strip_prefix("module.") {
        let (name, tail) = identifier(rest)?;
        declaration.push_str("module.");
        declaration.push_str(name);
        declaration.push('.');
        address = skip_instance_key(tail)?.strip_prefix('.')?;
    }
    for mode in ["data.", "ephemeral."] {
        if let Some(rest) = address.strip_prefix(mode) {
            address = rest;
            declaration.push_str(mode);
            break;
        }
    }
    let (kind, tail) = identifier(address)?;
    let (name, tail) = identifier(tail.strip_prefix('.')?)?;
    if !skip_instance_key(tail)?.is_empty() {
        return None;
    }
    declaration.push_str(kind);
    declaration.push('.');
    declaration.push_str(name);
    Some(declaration)
}

fn identifier(value: &str) -> Option<(&str, &str)> {
    let (name, tail) = value.split_at(value.find(['.', '[']).unwrap_or(value.len()));
    let mut chars = name.chars();
    (chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_alphabetic())
        && chars.all(|ch| {
            ch == '_' || ch == '-' || ch.is_alphanumeric() || !ch.is_ascii() && !ch.is_whitespace()
        }))
    .then_some((name, tail))
}

fn skip_instance_key(value: &str) -> Option<&str> {
    let Some(key) = value.strip_prefix('[') else {
        return Some(value);
    };
    if let Some(quoted) = key.strip_prefix('"') {
        let mut escaped = false;
        for (index, ch) in quoted.char_indices() {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                return quoted[index + 1..].strip_prefix(']');
            }
        }
        return None;
    }
    let (index, tail) = key.split_once(']')?;
    (!index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())).then_some(tail)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), &'static str> {
    if cancelled.load(Ordering::Relaxed) {
        Err("Terraform plan import cancelled")
    } else {
        Ok(())
    }
}
