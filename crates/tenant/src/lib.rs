/*
 * SPDX-FileCopyrightText: 2026 Stalwart contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 *
 * Clean-room multi-tenancy and white-label implementation.
 */

pub mod alert;
pub mod branding;
pub mod delivery;
pub mod edition;
pub mod llm;
pub mod masked;
pub mod retention;
pub mod sharding;

/// Opaque, stable identifier for a tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId(u32);

impl TenantId {
    /// Constructs a tenant id from its registry index. Mainly for callers that
    /// already hold a persisted index (and for tests).
    pub fn from_index(index: u32) -> Self {
        TenantId(index)
    }

    pub fn index(self) -> u32 {
        self.0
    }
}

/// Opaque identifier of a tenant-owned resource (account, domain, mailbox…).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResourceId(u64);

impl From<u64> for ResourceId {
    fn from(value: u64) -> Self {
        ResourceId(value)
    }
}

/// A registered tenant.
#[derive(Debug, Clone)]
pub struct Tenant {
    id: TenantId,
    name: String,
    max_resources: Option<u64>,
}

impl Tenant {
    pub fn id(&self) -> TenantId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Maximum number of resources this tenant may own, if capped.
    pub fn max_resources(&self) -> Option<u64> {
        self.max_resources
    }
}

/// Blueprint used to create a tenant.
#[derive(Debug, Clone)]
pub struct TenantSpec {
    name: String,
    max_resources: Option<u64>,
}

impl TenantSpec {
    pub fn new(name: impl Into<String>) -> Self {
        TenantSpec {
            name: name.into(),
            max_resources: None,
        }
    }

    /// Caps the number of resources this tenant may own.
    pub fn with_max_resources(mut self, max: u64) -> Self {
        self.max_resources = Some(max);
        self
    }
}

/// In-memory registry of tenants.
#[derive(Debug, Default)]
pub struct TenantStore {
    tenants: Vec<Tenant>,
    ownership: Vec<(ResourceId, TenantId)>,
}

impl TenantStore {
    pub fn new() -> Self {
        TenantStore::default()
    }

    pub fn create(&mut self, spec: TenantSpec) -> Result<TenantId, TenantError> {
        if self.tenants.iter().any(|t| t.name == spec.name) {
            return Err(TenantError::DuplicateName);
        }
        let id = TenantId(self.tenants.len() as u32);
        self.tenants.push(Tenant {
            id,
            name: spec.name,
            max_resources: spec.max_resources,
        });
        Ok(id)
    }

    pub fn get(&self, id: TenantId) -> Option<&Tenant> {
        self.tenants.iter().find(|t| t.id == id)
    }

    /// Records that `resource` belongs to `tenant`.
    pub fn assign(&mut self, tenant: TenantId, resource: ResourceId) -> Result<(), TenantError> {
        let max_resources = self.get(tenant).ok_or(TenantError::NotFound)?.max_resources;

        if self.ownership.iter().any(|(r, o)| *r == resource && *o == tenant) {
            // Idempotent re-assignment of an already-owned resource.
            return Ok(());
        }

        if let Some(max) = max_resources {
            let owned = self.ownership.iter().filter(|(_, o)| *o == tenant).count() as u64;
            if owned >= max {
                return Err(TenantError::QuotaExceeded);
            }
        }

        match self.ownership.iter_mut().find(|(r, _)| *r == resource) {
            Some((_, owner)) => *owner = tenant,
            None => self.ownership.push((resource, tenant)),
        }
        Ok(())
    }

    /// Grants access iff `tenant` owns `resource`.
    pub fn authorize(&self, tenant: TenantId, resource: ResourceId) -> Result<(), TenantError> {
        match self.ownership.iter().find(|(r, _)| *r == resource) {
            Some((_, owner)) if *owner == tenant => Ok(()),
            Some(_) => Err(TenantError::CrossTenantAccess),
            None => Err(TenantError::NotFound),
        }
    }
}

/// Errors returned by tenant operations.
#[derive(Debug, PartialEq, Eq)]
pub enum TenantError {
    DuplicateName,
    QuotaExceeded,
    NotFound,
    CrossTenantAccess,
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn given_a_new_tenant_when_created_then_it_can_be_retrieved_by_id() {
        let mut tenants = TenantStore::new();

        let id = tenants.create(TenantSpec::new("acme")).unwrap();

        let tenant = tenants.get(id).expect("tenant should exist");
        assert_eq!(tenant.name(), "acme");
        assert_eq!(tenant.id(), id);
    }

    #[test]
    fn given_two_tenants_when_created_then_ids_are_distinct() {
        let mut tenants = TenantStore::new();

        let acme = tenants.create(TenantSpec::new("acme")).unwrap();
        let globex = tenants.create(TenantSpec::new("globex")).unwrap();

        assert_ne!(acme, globex);
        assert_eq!(tenants.get(acme).unwrap().name(), "acme");
        assert_eq!(tenants.get(globex).unwrap().name(), "globex");
    }

    #[test]
    fn given_an_existing_name_when_creating_a_duplicate_then_it_is_rejected() {
        let mut tenants = TenantStore::new();
        tenants.create(TenantSpec::new("acme")).unwrap();

        let result = tenants.create(TenantSpec::new("acme"));

        assert_eq!(result, Err(TenantError::DuplicateName));
    }

    #[test]
    fn given_a_resource_owned_by_one_tenant_when_another_accesses_it_then_denied() {
        let mut tenants = TenantStore::new();
        let acme = tenants.create(TenantSpec::new("acme")).unwrap();
        let globex = tenants.create(TenantSpec::new("globex")).unwrap();
        let mailbox = ResourceId::from(42);
        tenants.assign(acme, mailbox).unwrap();

        assert_eq!(tenants.authorize(acme, mailbox), Ok(()));
        assert_eq!(
            tenants.authorize(globex, mailbox),
            Err(TenantError::CrossTenantAccess)
        );
    }

    #[test]
    fn given_an_unknown_resource_when_authorizing_then_not_found() {
        let mut tenants = TenantStore::new();
        let acme = tenants.create(TenantSpec::new("acme")).unwrap();

        assert_eq!(
            tenants.authorize(acme, ResourceId::from(999)),
            Err(TenantError::NotFound)
        );
    }

    #[test]
    fn given_a_tenant_at_its_resource_quota_when_assigning_more_then_rejected() {
        let mut tenants = TenantStore::new();
        let acme = tenants
            .create(TenantSpec::new("acme").with_max_resources(2))
            .unwrap();

        tenants.assign(acme, ResourceId::from(1)).unwrap();
        tenants.assign(acme, ResourceId::from(2)).unwrap();

        assert_eq!(
            tenants.assign(acme, ResourceId::from(3)),
            Err(TenantError::QuotaExceeded)
        );
    }

    #[test]
    fn given_a_tenant_without_a_quota_when_assigning_then_unlimited() {
        let mut tenants = TenantStore::new();
        let acme = tenants.create(TenantSpec::new("acme")).unwrap();

        for i in 0..100 {
            tenants.assign(acme, ResourceId::from(i)).unwrap();
        }
    }

    #[test]
    fn given_a_resource_at_quota_when_reassigning_the_same_one_then_allowed() {
        let mut tenants = TenantStore::new();
        let acme = tenants
            .create(TenantSpec::new("acme").with_max_resources(1))
            .unwrap();
        tenants.assign(acme, ResourceId::from(1)).unwrap();

        // Re-assigning an already-owned resource must not count against the quota.
        assert_eq!(tenants.assign(acme, ResourceId::from(1)), Ok(()));
    }
}
