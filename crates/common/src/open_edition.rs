/*
 * SPDX-FileCopyrightText: 2026 Stalwart contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 *
 * Free-edition capability API. This is the AGPL, always-on replacement for the
 * proprietary license gate: formerly-enterprise features are available in every
 * build with no license. New free-edition code paths consult this instead of
 * the SEL `is_enterprise_edition()` check, which is never referenced here.
 */

use crate::{Core, LogoCache, Server, USER_AGENT, manager::application::Resource};
use registry::schema::structs::{Domain, Tenant};
use tenant::TenantId;
use tenant::branding::{Branding, BrandingResolver};
use tenant::delivery::{DeliveryEvent, DeliveryQuery, DeliveryStatus};
use tenant::retention::RetentionPolicy;
use utils::HttpLimitResponse;

pub use tenant::delivery::{DeliveryQuery as OpenDeliveryQuery, DeliveryStatus as OpenDeliveryStatus};

pub use tenant::edition::Feature;

impl Core {
    /// Whether a formerly-enterprise `feature` is available. In the free
    /// edition every capability is always enabled.
    #[inline]
    pub fn open_feature_enabled(&self, _feature: Feature) -> bool {
        true
    }

    /// Free masked/aliased address: encodes `address_id` (with an optional
    /// absolute expiry, epoch seconds) into a tamper-evident email local-part.
    #[inline]
    pub fn open_masked_address(
        &self,
        address_id: u64,
        expires_at: Option<u32>,
        prefix: &str,
        domain: &str,
    ) -> String {
        tenant::masked::MaskedAddress::generate(address_id, expires_at, prefix, domain)
    }

    /// Recovers the address id from a masked local-part, rejecting tampering
    /// and any alias expired at `now` (epoch seconds).
    #[inline]
    pub fn open_parse_masked_address(&self, local_part: &str, now: u32) -> Option<u64> {
        tenant::masked::MaskedAddress::parse_local_part_at(local_part, now)
    }
}

impl Server {
    /// Free advanced telemetry: a machine-readable JSON snapshot of the current
    /// counters, gauges and histograms. Always available (all metrics, no
    /// license), reading the AGPL `trc::Collector`.
    pub fn open_metrics_snapshot(&self) -> String {
        use trc::Collector;

        let counters: Vec<_> = Collector::collect_counters(true)
            .map(|c| {
                serde_json::json!({
                    "id": c.id().as_str(),
                    "description": c.id().description(),
                    "value": c.value(),
                })
            })
            .collect();

        let gauges: Vec<_> = Collector::collect_gauges(true)
            .map(|g| {
                serde_json::json!({
                    "id": g.id().as_str(),
                    "description": g.id().description(),
                    "value": g.get(),
                })
            })
            .collect();

        let histograms: Vec<_> = Collector::collect_histograms(true)
            .map(|h| {
                serde_json::json!({
                    "id": h.id().as_str(),
                    "description": h.id().description(),
                    "count": h.count(),
                    "sum": h.sum(),
                })
            })
            .collect();

        serde_json::json!({
            "counters": counters,
            "gauges": gauges,
            "histograms": histograms,
        })
        .to_string()
    }

    /// Free delivery history: records one delivery outcome. Always-on.
    pub fn open_record_delivery(
        &self,
        message_id: u64,
        tenant: Option<u32>,
        recipient: impl Into<String>,
        status: DeliveryStatus,
        timestamp: u64,
    ) {
        let event = DeliveryEvent::new(
            message_id,
            TenantId::from_index(tenant.unwrap_or(0)),
            recipient,
            status,
            timestamp,
        );
        self.inner.data.delivery_history.lock().record(event);
    }

    /// Free delivery history: returns matching events as a JSON array.
    pub fn open_query_delivery(&self, query: &DeliveryQuery) -> String {
        let history = self.inner.data.delivery_history.lock();
        let events: Vec<_> = history
            .query(query)
            .into_iter()
            .map(|e| {
                serde_json::json!({
                    "messageId": e.message_id(),
                    "tenant": e.tenant().index(),
                    "recipient": e.recipient(),
                    "status": match e.status() {
                        DeliveryStatus::Delivered => "delivered",
                        DeliveryStatus::Bounced => "bounced",
                        DeliveryStatus::Deferred => "deferred",
                    },
                    "timestamp": e.timestamp(),
                })
            })
            .collect();
        serde_json::Value::Array(events).to_string()
    }

    /// Purges delivery-history events aged out under `policy` as of `now`
    /// (epoch seconds); returns the number removed.
    pub fn open_purge_delivery(&self, retention_seconds: u64, now: u64) -> usize {
        self.inner
            .data
            .delivery_history
            .lock()
            .purge(RetentionPolicy::of_seconds(retention_seconds), now)
    }
}

impl Server {
    /// Free white-label: resolves the effective logo URL for `domain`, layering
    /// a domain-specific override over the tenant's branding over none.
    ///
    /// Always available — this path never consults the SEL license gate; the
    /// layering is performed by the AGPL [`BrandingResolver`].
    pub async fn open_logo_url(&self, domain: &str) -> trc::Result<Option<String>> {
        let Some(domain_cache) = self.domain(domain).await? else {
            return Ok(None);
        };
        let tenant_id = domain_cache.id_tenant;

        let domain_logo = self
            .registry()
            .object::<Domain>(domain_cache.id.into())
            .await?
            .and_then(|d| d.logo);

        let tenant_logo = match tenant_id {
            Some(tid) => self
                .registry()
                .object::<Tenant>(tid.into())
                .await?
                .and_then(|t| t.logo),
            None => None,
        };

        // Layer with the AGPL resolver: default -> tenant -> domain override.
        let tenant_key = TenantId::from_index(tenant_id.unwrap_or(0));
        let mut resolver = BrandingResolver::new(Branding::default());
        if let Some(logo) = tenant_logo {
            resolver.set_tenant(tenant_key, Branding::default().with_logo_url(logo));
        }
        resolver.map_domain(domain, tenant_key);
        if let Some(logo) = domain_logo {
            resolver.set_domain(domain, Branding::default().with_logo_url(logo));
        }

        Ok(resolver
            .resolve(domain)
            .logo_url()
            .map(|url| url.to_string()))
    }

    /// Free white-label: serves the effective logo image for `domain`,
    /// downloading it (bounded) from the resolved URL and caching the result.
    ///
    /// Always available and fully AGPL — it resolves the URL via
    /// [`Server::open_logo_url`] and never invokes the SEL `logo_resource`.
    pub async fn open_logo_resource(
        &self,
        domain: &str,
    ) -> trc::Result<Option<Resource<Vec<u8>>>> {
        const MAX_IMAGE_SIZE: usize = 1024 * 1024;

        let domain = psl::domain_str(domain).unwrap_or(domain);

        // Serve from cache when present.
        if let Some(cached) = { self.inner.data.logos.lock().get(domain).cloned() } {
            return Ok(cached.data);
        }

        let Some(logo_url) = self.open_logo_url(domain).await? else {
            return Ok(None);
        };

        let response = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .unwrap()
            .get(logo_url.as_str())
            .send()
            .await
            .map_err(|err| {
                trc::ResourceEvent::DownloadExternal
                    .into_err()
                    .details("Failed to download logo")
                    .reason(err)
            })?;

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|ct| ct.to_str().ok())
            .unwrap_or("image/svg+xml")
            .to_string();

        let contents = response
            .bytes_with_limit(MAX_IMAGE_SIZE)
            .await
            .map_err(|err| {
                trc::ResourceEvent::DownloadExternal
                    .into_err()
                    .details("Failed to download logo")
                    .reason(err)
            })?
            .ok_or_else(|| {
                trc::ResourceEvent::DownloadExternal
                    .into_err()
                    .details("Download exceeded maximum size")
            })?;

        let resource = Resource::new(content_type, contents);
        self.inner.data.logos.lock().insert(
            domain.into(),
            LogoCache {
                domain_id: u32::MAX,
                tenant_id: None,
                data: Some(resource.clone()),
            },
        );

        Ok(Some(resource))
    }
}
