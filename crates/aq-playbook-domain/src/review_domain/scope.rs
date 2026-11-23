//! Validate grant scope under the selected host catalog and installed extensions.
use super::*;
fn validate_bounds(scope: &crate::GrantScope) -> Result<()> {
    if scope.workspace_id.is_nil() || scope.project_id.is_nil() {
        return Err(Error::Invalid);
    }
    validate_size(scope)
}
fn validate_size(scope: &crate::GrantScope) -> Result<()> {
    let bytes = aq_playbook_host::model::canonical_bytes(scope).map_err(|_| Error::Invalid)?;
    if bytes.len() > 32768 {
        return Err(Error::Invalid);
    }
    Ok(())
}
impl CatalogProfile {
    pub fn validate_scope(self, scope: &crate::GrantScope) -> Result<()> {
        self.validate_scope_with_extensions(scope, |_| false)
    }
    pub fn validate_scope_with_extensions(
        self,
        scope: &crate::GrantScope,
        installed: impl Fn(&str) -> bool,
    ) -> Result<()> {
        let mut base_capabilities = scope.capabilities.clone();
        base_capabilities.retain(|capability| {
            !aq_playbook_host::plugins::extension_capability(capability).is_some_and(&installed)
        });
        let has_extensions = base_capabilities != scope.capabilities;
        if has_extensions {
            validate_bounds(scope)?;
            if base_capabilities.is_empty() && scope.recipients.is_empty() {
                return Ok(());
            }
        }
        if self == Self::Mail1 {
            let mut base = scope.clone();
            base.capabilities = base_capabilities;
            return base.validate();
        }
        if self.is_host() {
            return self.validate_host_scope(scope, &base_capabilities);
        }
        let mut base = scope.clone();
        base.capabilities = BTreeSet::from(["message.send".into()]);
        base.validate()?;
        let allowed = BTreeSet::from([
            "message.send".into(),
            "decision.request".into(),
            "operation.get".into(),
        ]);
        if scope.capabilities.is_empty() || !base_capabilities.is_subset(&allowed) {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    fn validate_host_scope(
        self,
        scope: &crate::GrantScope,
        base_capabilities: &BTreeSet<String>,
    ) -> Result<()> {
        let ids_present = !scope.workspace_id.is_nil() && !scope.project_id.is_nil();
        if !ids_present || !scope.recipients.is_empty() {
            return Err(Error::Invalid);
        }
        let allowed = self.allowed_capabilities();
        if scope.capabilities.is_empty() || !base_capabilities.is_subset(&allowed) {
            return Err(Error::Invalid);
        }
        validate_size(scope)
    }
    fn allowed_capabilities(self) -> BTreeSet<String> {
        let mut allowed: BTreeSet<String> = if self.has_routing() {
            HOST9_CAPABILITIES.iter().map(|c| c.to_string()).collect()
        } else if self.is_host2() {
            HOST7_CAPABILITIES.iter().map(|c| c.to_string()).collect()
        } else {
            HOST5_CAPABILITIES.iter().map(|c| c.to_string()).collect()
        };
        if self == Self::Monitoring6 {
            allowed.insert("findings.reconcile".into());
        }
        if self.has_notes() {
            allowed.insert(WORK_ACTIVATE.into());
        }
        #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
        if self.has_environments() {
            allowed.extend(ENVIRONMENT_COMMANDS.iter().map(|c| c.to_string()));
        }
        #[cfg(feature = "delivery_playbook")]
        if self.is_delivery() {
            allowed.extend(HOST10_DELIVERY_CAPABILITIES.iter().map(|c| c.to_string()));
        }
        allowed
    }
    pub fn authority(
        self,
        id: Uuid,
        revision: i64,
        scope: &crate::GrantScope,
    ) -> Result<Authority> {
        self.authority_with_extensions(id, revision, scope, |_| false)
    }
    pub fn authority_with_extensions(
        self,
        id: Uuid,
        revision: i64,
        scope: &crate::GrantScope,
        installed: impl Fn(&str) -> bool,
    ) -> Result<Authority> {
        self.validate_scope_with_extensions(scope, installed)?;
        let mut authority = crate::authority(id, revision);
        authority.capabilities = scope.capabilities.clone();
        Ok(authority)
    }
}
