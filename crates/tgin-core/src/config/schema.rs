//! JSON Schemas of the config types with hand-written serde impls.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};

use super::model::{
    At, ByteSize, Cidr, HumanDuration, IpAllowlist, Name, RawConfig, SecretText, Split, Target,
    Template, WebhookSecret, WorkerThreads,
};

/// What [`Name`] accepts; `validate` checks the same rule.
pub(super) const NAME_PATTERN: &str = "^[a-z0-9][a-z0-9-]{0,62}$";

/// The JSON Schema (draft 2020-12) of the config file, pretty-printed.
pub fn schema() -> String {
    tgin_runtime::config::schema_json::<RawConfig>()
}

/// A located value has the schema of the value.
impl<T: JsonSchema> JsonSchema for At<T> {
    fn inline_schema() -> bool {
        T::inline_schema()
    }

    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }

    fn schema_id() -> Cow<'static, str> {
        T::schema_id()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        T::json_schema(generator)
    }
}

impl JsonSchema for Name {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "Name".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": NAME_PATTERN })
    }
}

impl JsonSchema for SecretText {
    fn schema_name() -> Cow<'static, str> {
        "Secret".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "A secret. Supports `${env:NAME}` and `${file:PATH}` (relative to the config file); `$${` is a literal `${`.",
            "type": "string",
            "minLength": 1,
            "writeOnly": true
        })
    }
}

impl JsonSchema for Template {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "Template".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "minLength": 1 })
    }
}

impl JsonSchema for HumanDuration {
    fn schema_name() -> Cow<'static, str> {
        "Duration".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "A duration such as `500ms`, `1.5s`, `5m` or `1h 30m`, or `0`. Month (`M`) and year (`y`) units are rejected.",
            "anyOf": [
                {
                    "type": "string",
                    "pattern": "^(0|\\s*([0-9]+(\\.[0-9]+)?\\s*(nanos|nsec|ns|usec|us|µs|millis|msec|ms|seconds|second|secs|sec|s|minutes|minute|mins|min|m|hours|hour|hrs|hr|h|days|day|d|weeks|week|wks|wk|w)\\s*)+)$"
                },
                { "const": 0 }
            ],
            "examples": ["30s", "1.5s", "5m"]
        })
    }
}

impl JsonSchema for ByteSize {
    fn schema_name() -> Cow<'static, str> {
        "ByteSize".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "A size of at least 1 byte: bytes, or a number with `B`, `KiB`, `MiB` or `GiB`.",
            "oneOf": [
                { "type": "integer", "minimum": 1 },
                { "type": "string", "pattern": "^\\s*0*[1-9][0-9]*\\s*(B|KiB|MiB|GiB)?\\s*$" }
            ],
            "examples": ["64MiB", "512KiB", 1048576]
        })
    }
}

impl JsonSchema for Cidr {
    fn schema_name() -> Cow<'static, str> {
        "Cidr".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "An IP network such as `10.0.0.0/8`, or a single address.",
            "type": "string",
            "examples": ["10.0.0.0/8", "2001:db8::/32"]
        })
    }
}

impl JsonSchema for WorkerThreads {
    fn schema_name() -> Cow<'static, str> {
        "WorkerThreads".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "oneOf": [
                { "const": "auto", "description": "One per CPU." },
                { "type": "integer", "minimum": 1 }
            ]
        })
    }
}

impl JsonSchema for WebhookSecret {
    fn schema_name() -> Cow<'static, str> {
        "WebhookSecret".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let secret = generator.subschema_for::<SecretText>();
        json_schema!({
            "anyOf": [
                { "const": "auto", "description": "Generate a secret at start." },
                { "const": "none", "description": "Send and check no secret." },
                secret
            ]
        })
    }
}

impl JsonSchema for IpAllowlist {
    fn schema_name() -> Cow<'static, str> {
        "IpAllowlist".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let networks = generator.subschema_for::<Vec<Cidr>>();
        json_schema!({
            "oneOf": [
                { "const": "none", "description": "Any client may post." },
                { "const": "telegram", "description": "Telegram's published webhook networks." },
                networks
            ]
        })
    }
}

impl JsonSchema for Target {
    fn schema_name() -> Cow<'static, str> {
        "Target".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let split = generator.subschema_for::<Split>();
        let children = generator.subschema_for::<Vec<Target>>();
        json_schema!({
            "description": "A consumer name, `{split: …}` (one child receives the update) or `{all: […]}` (every child does).",
            "oneOf": [
                { "type": "string", "description": "A consumer name." },
                {
                    "type": "object",
                    "properties": { "split": split },
                    "required": ["split"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": { "all": children },
                    "required": ["all"],
                    "additionalProperties": false
                }
            ]
        })
    }
}
