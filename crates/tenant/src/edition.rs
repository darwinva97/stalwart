/*
 * SPDX-FileCopyrightText: 2026 Stalwart contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 *
 * The free edition: a single, always-on entry point that aggregates every
 * reimplemented capability. This is the AGPL replacement for the proprietary
 * license gate — here every feature is available, in every build, at no cost.
 */

use crate::TenantStore;
use crate::branding::{Branding, BrandingResolver};
use crate::delivery::DeliveryHistory;

/// A capability that used to sit behind the enterprise license.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    MultiTenancy,
    WhiteLabel,
    DeliveryHistory,
    Telemetry,
    Sharding,
}

/// The free edition. Aggregates every reimplemented capability and reports all
/// of them as enabled — there is no paid tier and no license to check.
#[derive(Debug)]
pub struct OpenEdition {
    pub tenants: TenantStore,
    pub branding: BrandingResolver,
    pub delivery: DeliveryHistory,
}

impl Default for OpenEdition {
    fn default() -> Self {
        OpenEdition::new()
    }
}

impl OpenEdition {
    pub fn new() -> Self {
        OpenEdition {
            tenants: TenantStore::new(),
            branding: BrandingResolver::new(Branding::default()),
            delivery: DeliveryHistory::new(),
        }
    }

    /// Builds a free edition with a default (fallback) branding.
    pub fn with_branding(branding: Branding) -> Self {
        OpenEdition {
            branding: BrandingResolver::new(branding),
            ..OpenEdition::new()
        }
    }

    /// Every feature is always available in the free edition.
    pub fn is_feature_enabled(&self, _feature: Feature) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TenantSpec;
    use crate::branding::Branding;

    #[test]
    fn given_the_free_edition_when_asked_then_every_feature_is_enabled() {
        let edition = OpenEdition::new();

        assert!(edition.is_feature_enabled(Feature::MultiTenancy));
        assert!(edition.is_feature_enabled(Feature::WhiteLabel));
        assert!(edition.is_feature_enabled(Feature::DeliveryHistory));
        assert!(edition.is_feature_enabled(Feature::Telemetry));
        assert!(edition.is_feature_enabled(Feature::Sharding));
    }

    #[test]
    fn given_the_free_edition_when_creating_a_tenant_then_it_works_without_a_license() {
        let mut edition = OpenEdition::new();

        let id = edition.tenants.create(TenantSpec::new("acme")).unwrap();

        assert_eq!(edition.tenants.get(id).unwrap().name(), "acme");
    }

    #[test]
    fn given_the_free_edition_when_resolving_branding_then_it_is_available() {
        let edition =
            OpenEdition::with_branding(Branding::default().with_product_name("Free Mail"));

        assert_eq!(
            edition.branding.resolve("any.example").product_name(),
            Some("Free Mail")
        );
    }
}
