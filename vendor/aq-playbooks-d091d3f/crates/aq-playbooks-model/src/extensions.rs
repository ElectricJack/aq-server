use super::{ModelError, Recovery, Registry, Schema, valid_name};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// The contracts and authoring notes an installed plugin contributes. Package
/// versions are transport provenance and do not belong in this view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ExtensionFields")]
pub struct CatalogExtension {
    name: String,
    registry: Registry,
    notes: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtensionFields {
    name: String,
    registry: Registry,
    notes: Value,
}

impl TryFrom<ExtensionFields> for CatalogExtension {
    type Error = ModelError;

    fn try_from(fields: ExtensionFields) -> Result<Self, Self::Error> {
        Self::new(fields.name, fields.registry, fields.notes)
    }
}

impl CatalogExtension {
    pub fn new(
        name: impl Into<String>,
        registry: Registry,
        notes: Value,
    ) -> Result<Self, ModelError> {
        let name = name.into();
        if !(2..=32).contains(&name.len())
            || !name.as_bytes()[0].is_ascii_lowercase()
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(ModelError::Invalid(format!(
                "invalid extension name {name:?}"
            )));
        }
        let capability = format!("ext.{name}");
        let prefix = format!("{capability}.");
        let valid_registration = |key: &str, name: &str| {
            key == name
                && valid_name(name)
                && name.strip_prefix(&prefix).is_some_and(|s| !s.is_empty())
        };
        for (key, command) in &registry.commands {
            if !valid_registration(key, &command.name) {
                return Err(ModelError::Invalid(format!(
                    "extension {name}: command {key:?} must have a matching name under {prefix}"
                )));
            }
            let refused = command.outcomes.get("refused");
            if !matches!(refused, Some(Schema::Object { properties, required })
                if properties.len() == 1 && required.len() == 1 && required.contains("reason")
                && matches!(properties.get("reason"), Some(Schema::String { max_length, values })
                    if *max_length >= "invalid_plugin_response".len() && values.is_empty()))
            {
                return Err(ModelError::Invalid(format!(
                    "extension {name}: command {key:?} requires refused {{reason: string}} with open values that admit host refusals"
                )));
            }
            if command.recovery != Recovery::KeyedIdempotent
                || !only(&command.scopes, "project")
                || !only(&command.capabilities, &capability)
            {
                return Err(ModelError::Invalid(format!(
                    "extension {name}: command {key:?} requires keyed_idempotent recovery, project scope and capability {capability}"
                )));
            }
            if command.asynchronous.is_some() || command.cancellation {
                return Err(ModelError::Invalid(format!(
                    "extension {name}: command {key:?} must be synchronous and cannot be cancelled"
                )));
            }
            if command.version.is_empty()
                || command.version.len() > 64
                || command.outcomes.len() > 64
                || !(1..=super::MAX_ARTIFACT_BYTES).contains(&command.max_payload_bytes)
                || !(2..=31_536_000_000).contains(&command.deadline_ms)
            {
                return Err(ModelError::Invalid(format!(
                    "extension {name}: command {key:?} has invalid version or resource bounds"
                )));
            }
            command.input.validate_definition()?;
            for outcome in command.outcomes.values() {
                outcome.validate_definition()?;
            }
        }
        for (key, event) in &registry.events {
            if registry.commands.contains_key(key) {
                return Err(ModelError::Invalid(format!(
                    "extension {name}: duplicate registration {key}"
                )));
            }
            if !valid_registration(key, &event.name) {
                return Err(ModelError::Invalid(format!(
                    "extension {name}: event {key:?} must have a matching name under {prefix}"
                )));
            }
            if !only(&event.trusted_producers, &capability) || !only(&event.scopes, "project") {
                return Err(ModelError::Invalid(format!(
                    "extension {name}: event {key:?} requires project scope and trusted producer {capability}"
                )));
            }
            if event.version.is_empty()
                || !(1..=super::MAX_ARTIFACT_BYTES).contains(&event.max_payload_bytes)
                || event.retention_ms == 0
            {
                return Err(ModelError::Invalid(format!(
                    "extension {name}: event {key:?} has invalid version or resource bounds"
                )));
            }
            event.payload.validate_definition()?;
            if !matches!(
                event
                    .subject_field
                    .as_deref()
                    .and_then(|path| required_field(&event.payload, path)),
                Some(Schema::String { .. })
            ) || !matches!(event.revision_field.as_deref().and_then(|path| required_field(&event.payload, path)), Some(Schema::Integer { minimum, .. }) if *minimum >= 0)
                || event.correlation_fields.iter().any(|path| {
                    !matches!(
                        required_field(&event.payload, path),
                        Some(Schema::String { .. })
                    )
                })
            {
                return Err(ModelError::Invalid(format!(
                    "extension {name}: event {key:?} requires a string subject, non-negative integer revision and string correlation fields in its payload"
                )));
            }
        }
        if !notes.is_object()
            || ["commands", "events"]
                .into_iter()
                .any(|key| notes.get(key).is_some_and(|value| !value.is_object()))
        {
            return Err(ModelError::Invalid(format!(
                "extension {name}: catalog notes must be an object with command and event maps"
            )));
        }
        Ok(Self {
            name,
            registry,
            notes,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn notes(&self) -> &Value {
        &self.notes
    }
}

fn only(values: &BTreeSet<String>, value: &str) -> bool {
    values.len() == 1 && values.contains(value)
}

fn required_field<'a>(schema: &'a Schema, path: &str) -> Option<&'a Schema> {
    path.split('.')
        .try_fold(schema, |schema, field| match schema {
            Schema::Object {
                properties,
                required,
            } if required.contains(field) => properties.get(field),
            _ => None,
        })
}

impl Registry {
    /// Compose an independent view without mutating the base or changing any
    /// contract. A command or event name must belong to one catalog only.
    pub fn with_extensions(&self, extensions: &[CatalogExtension]) -> Result<Self, ModelError> {
        let mut registry = self.clone();
        let mut names = BTreeSet::new();
        for extension in extensions {
            if !names.insert(extension.name()) {
                return Err(ModelError::Invalid(format!(
                    "duplicate extension {}",
                    extension.name()
                )));
            }
            for name in extension
                .registry
                .commands
                .keys()
                .chain(extension.registry.events.keys())
            {
                if registry.commands.contains_key(name) || registry.events.contains_key(name) {
                    return Err(ModelError::Invalid(format!(
                        "extension {}: duplicate registration {name}",
                        extension.name()
                    )));
                }
            }
            registry
                .commands
                .extend(extension.registry.commands.clone());
            registry.events.extend(extension.registry.events.clone());
        }
        Ok(registry)
    }
}
