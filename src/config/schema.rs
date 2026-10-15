use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny};
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CladdingConfigDocument {
    pub name: String,
    #[serde(default)]
    pub use_runsc: bool,
    pub agent: AgentDocument,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub nw_sandbox: Option<ComponentDocument>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub fs_sandbox: Option<ComponentDocument>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub proxy: Option<ProxyDocument>,
    #[serde(default)]
    pub mounts: Vec<MountDocument>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentDocument {
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub image: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub init: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub build: Option<ImageBuildDocument>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub security_opts: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub enabled: Option<PresentField>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ComponentDocument {
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub image: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub enabled: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub init: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub build: Option<ImageBuildDocument>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub security_opts: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProxyDocument {
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub image: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub build: Option<ImageBuildDocument>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub agent: Option<SessionConfigDocument>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub nw_sandbox: Option<SessionConfigDocument>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionConfigDocument {
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub session_config: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ImageBuildDocument {
    pub containerfile: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub context: Option<String>,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MountDocument {
    pub mount: String,
    #[serde(
        default,
        rename = "hostPath",
        deserialize_with = "deserialize_non_null_option"
    )]
    pub host_path: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub volume: Option<String>,
    #[serde(
        default,
        rename = "type",
        deserialize_with = "deserialize_non_null_option"
    )]
    pub kind: Option<MountKind>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub size: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub targets: Option<Vec<String>>,
    #[serde(default)]
    pub ignore: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum MountKind {
    Bind,
    Readonly,
    Copy,
    Tmpfs,
}

#[derive(Debug)]
pub(super) struct PresentField;

fn deserialize_non_null_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn deserialize_present<'de, D>(deserializer: D) -> Result<Option<PresentField>, D::Error>
where
    D: Deserializer<'de>,
{
    IgnoredAny::deserialize(deserializer)?;
    Ok(Some(PresentField))
}
