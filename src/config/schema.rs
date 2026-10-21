use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor, value::MapAccessDeserializer};
use std::collections::BTreeMap;
use std::fmt;

// Derived structs accept positional sequences as well as maps. These config
// documents were historically objects, so deserialize_map keeps that shape
// while the generated fields structs retain typed fields and strict keys.
macro_rules! object_document {
    (
        $document:ident, $fields:ident {
            $( $(#[$attribute:meta])* $field:ident: $field_type:ty ),* $(,)?
        }
    ) => {
        #[derive(Debug)]
        pub(super) struct $document {
            $(pub(super) $field: $field_type),*
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct $fields {
            $( $(#[$attribute])* $field: $field_type ),*
        }

        impl<'de> Deserialize<'de> for $document {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct ObjectVisitor;

                impl<'de> Visitor<'de> for ObjectVisitor {
                    type Value = $document;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a configuration object")
                    }

                    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
                    where
                        A: MapAccess<'de>,
                    {
                        let fields = $fields::deserialize(MapAccessDeserializer::new(map))?;
                        Ok($document {
                            $($field: fields.$field),*
                        })
                    }
                }

                deserializer.deserialize_map(ObjectVisitor)
            }
        }
    };
}

object_document!(CladdingConfigDocument, CladdingConfigDocumentFields {
    name: String,
    #[serde(default)]
    use_runsc: bool,
    agent: AgentDocument,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    nw_sandbox: Option<ComponentDocument>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    fs_sandbox: Option<ComponentDocument>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    proxy: Option<ProxyDocument>,
    #[serde(default)]
    mounts: Vec<MountDocument>,
    #[serde(default)]
    fake_credential_env: BTreeMap<String, String>,
});

object_document!(AgentDocument, AgentDocumentFields {
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    image: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    init: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    build: Option<ImageBuildDocument>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    security_opts: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    enabled: Option<PresentField>,
});

object_document!(ComponentDocument, ComponentDocumentFields {
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    image: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    enabled: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    init: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    build: Option<ImageBuildDocument>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    security_opts: Option<Vec<String>>,
});

object_document!(ProxyDocument, ProxyDocumentFields {
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    image: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    build: Option<ImageBuildDocument>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    agent: Option<SessionConfigDocument>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    nw_sandbox: Option<SessionConfigDocument>,
});

object_document!(SessionConfigDocument, SessionConfigDocumentFields {
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    session_config: Option<String>,
});

object_document!(ImageBuildDocument, ImageBuildDocumentFields {
    containerfile: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    context: Option<String>,
    #[serde(default)]
    args: BTreeMap<String, String>,
});

object_document!(MountDocument, MountDocumentFields {
    mount: String,
    #[serde(
        default,
        rename = "hostPath",
        deserialize_with = "deserialize_non_null_option"
    )]
    host_path: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    volume: Option<String>,
    #[serde(
        default,
        rename = "type",
        deserialize_with = "deserialize_non_null_option"
    )]
    kind: Option<MountKind>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    size: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    targets: Option<Vec<String>>,
    #[serde(default)]
    ignore: bool,
});

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
