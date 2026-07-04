/*
 * SPDX-FileCopyrightText: 2026 Stalwart contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 *
 * White-label branding resolution.
 */

use crate::TenantId;

/// White-label branding attributes. Every field is optional so that partial
/// overrides can be layered over one another.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Branding {
    product_name: Option<String>,
    logo_url: Option<String>,
    primary_color: Option<String>,
}

impl Branding {
    pub fn with_product_name(mut self, value: impl Into<String>) -> Self {
        self.product_name = Some(value.into());
        self
    }

    pub fn with_logo_url(mut self, value: impl Into<String>) -> Self {
        self.logo_url = Some(value.into());
        self
    }

    pub fn with_primary_color(mut self, value: impl Into<String>) -> Self {
        self.primary_color = Some(value.into());
        self
    }

    pub fn product_name(&self) -> Option<&str> {
        self.product_name.as_deref()
    }

    pub fn logo_url(&self) -> Option<&str> {
        self.logo_url.as_deref()
    }

    pub fn primary_color(&self) -> Option<&str> {
        self.primary_color.as_deref()
    }

    /// Overlays `other` on top of `self`: every field set in `other` replaces
    /// the corresponding field here; unset fields are left untouched.
    fn overlay(&mut self, other: &Branding) {
        if other.product_name.is_some() {
            self.product_name = other.product_name.clone();
        }
        if other.logo_url.is_some() {
            self.logo_url = other.logo_url.clone();
        }
        if other.primary_color.is_some() {
            self.primary_color = other.primary_color.clone();
        }
    }
}

/// Resolves the effective branding for a domain by layering
/// default -> tenant -> domain overrides.
#[derive(Debug, Clone)]
pub struct BrandingResolver {
    default: Branding,
    tenant_branding: Vec<(TenantId, Branding)>,
    domain_tenant: Vec<(String, TenantId)>,
    domain_branding: Vec<(String, Branding)>,
}

impl BrandingResolver {
    pub fn new(default: Branding) -> Self {
        BrandingResolver {
            default,
            tenant_branding: Vec::new(),
            domain_tenant: Vec::new(),
            domain_branding: Vec::new(),
        }
    }

    /// Sets the branding for a whole tenant.
    pub fn set_tenant(&mut self, tenant: TenantId, branding: Branding) {
        upsert(&mut self.tenant_branding, tenant, branding);
    }

    /// Records that `domain` belongs to `tenant`.
    pub fn map_domain(&mut self, domain: impl Into<String>, tenant: TenantId) {
        upsert(&mut self.domain_tenant, domain.into(), tenant);
    }

    /// Sets a domain-specific branding override.
    pub fn set_domain(&mut self, domain: impl Into<String>, branding: Branding) {
        upsert(&mut self.domain_branding, domain.into(), branding);
    }

    /// Returns the fully-resolved branding for `domain`.
    pub fn resolve(&self, domain: &str) -> Branding {
        let mut effective = self.default.clone();

        if let Some((_, tenant)) = self.domain_tenant.iter().find(|(d, _)| d == domain)
            && let Some((_, branding)) = self.tenant_branding.iter().find(|(t, _)| t == tenant)
        {
            effective.overlay(branding);
        }

        if let Some((_, branding)) = self.domain_branding.iter().find(|(d, _)| d == domain) {
            effective.overlay(branding);
        }

        effective
    }
}

/// Inserts or replaces the value keyed by `key`.
fn upsert<K: PartialEq, V>(store: &mut Vec<(K, V)>, key: K, value: V) {
    match store.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = value,
        None => store.push((key, value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_only_a_default_when_resolving_an_unknown_domain_then_default_is_returned() {
        let resolver = BrandingResolver::new(Branding::default().with_product_name("Stalwart"));

        let effective = resolver.resolve("unknown.example");

        assert_eq!(effective.product_name(), Some("Stalwart"));
    }

    #[test]
    fn given_a_tenant_brand_when_resolving_that_tenants_domain_then_it_overlays_the_default() {
        let acme = TenantId::from_index(0);
        let mut resolver =
            BrandingResolver::new(Branding::default().with_product_name("Stalwart"));
        resolver.set_tenant(acme, Branding::default().with_logo_url("https://acme/logo.svg"));
        resolver.map_domain("acme.example", acme);

        let effective = resolver.resolve("acme.example");

        // logo comes from the tenant, product name falls through to the default.
        assert_eq!(effective.logo_url(), Some("https://acme/logo.svg"));
        assert_eq!(effective.product_name(), Some("Stalwart"));
    }

    #[test]
    fn given_a_domain_override_when_resolving_then_it_wins_over_tenant_and_default() {
        let acme = TenantId::from_index(0);
        let mut resolver =
            BrandingResolver::new(Branding::default().with_product_name("Stalwart"));
        resolver.set_tenant(
            acme,
            Branding::default().with_product_name("Acme Mail").with_logo_url("tenant.svg"),
        );
        resolver.map_domain("mail.acme.example", acme);
        resolver.set_domain("mail.acme.example", Branding::default().with_logo_url("domain.svg"));

        let effective = resolver.resolve("mail.acme.example");

        assert_eq!(effective.logo_url(), Some("domain.svg")); // domain wins
        assert_eq!(effective.product_name(), Some("Acme Mail")); // from tenant
    }
}
