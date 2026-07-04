/*
 * SPDX-FileCopyrightText: 2026 Stalwart contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 *
 * Delivery history: an append-only, queryable log of message delivery
 * outcomes, scoped per tenant.
 */

use crate::TenantId;

/// Final outcome of a delivery attempt to a single recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStatus {
    Delivered,
    Bounced,
    Deferred,
}

/// One recorded delivery outcome.
#[derive(Debug, Clone)]
pub struct DeliveryEvent {
    message_id: u64,
    tenant: TenantId,
    recipient: String,
    status: DeliveryStatus,
    timestamp: u64,
}

impl DeliveryEvent {
    pub fn new(
        message_id: u64,
        tenant: TenantId,
        recipient: impl Into<String>,
        status: DeliveryStatus,
        timestamp: u64,
    ) -> Self {
        DeliveryEvent {
            message_id,
            tenant,
            recipient: recipient.into(),
            status,
            timestamp,
        }
    }

    pub fn message_id(&self) -> u64 {
        self.message_id
    }

    pub fn tenant(&self) -> TenantId {
        self.tenant
    }

    pub fn recipient(&self) -> &str {
        &self.recipient
    }

    pub fn status(&self) -> DeliveryStatus {
        self.status
    }

    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }
}

/// A filter over the delivery history. Every criterion is optional; an unset
/// criterion matches everything.
#[derive(Debug, Clone, Default)]
pub struct DeliveryQuery {
    tenant: Option<TenantId>,
    recipient: Option<String>,
    status: Option<DeliveryStatus>,
    since: Option<u64>,
    until: Option<u64>,
}

impl DeliveryQuery {
    /// Matches every event belonging to `tenant`.
    pub fn for_tenant(tenant: TenantId) -> Self {
        DeliveryQuery {
            tenant: Some(tenant),
            ..Default::default()
        }
    }

    /// Restricts to a single recipient address.
    pub fn recipient(mut self, recipient: impl Into<String>) -> Self {
        self.recipient = Some(recipient.into());
        self
    }

    /// Restricts to a single delivery status.
    pub fn status(mut self, status: DeliveryStatus) -> Self {
        self.status = Some(status);
        self
    }

    /// Lower bound (inclusive) of the timestamp window.
    pub fn since(mut self, since: u64) -> Self {
        self.since = Some(since);
        self
    }

    /// Upper bound (exclusive) of the timestamp window.
    pub fn until(mut self, until: u64) -> Self {
        self.until = Some(until);
        self
    }

    fn matches(&self, event: &DeliveryEvent) -> bool {
        self.tenant.is_none_or(|t| event.tenant == t)
            && self
                .recipient
                .as_deref()
                .is_none_or(|r| event.recipient == r)
            && self.status.is_none_or(|s| event.status == s)
            && self.since.is_none_or(|s| event.timestamp >= s)
            && self.until.is_none_or(|u| event.timestamp < u)
    }
}

/// Append-only store of delivery events.
#[derive(Debug, Default)]
pub struct DeliveryHistory {
    events: Vec<DeliveryEvent>,
}

impl DeliveryHistory {
    pub fn new() -> Self {
        DeliveryHistory::default()
    }

    pub fn record(&mut self, event: DeliveryEvent) {
        self.events.push(event);
    }

    /// Returns every event matching `query`, in insertion order.
    pub fn query(&self, query: &DeliveryQuery) -> Vec<&DeliveryEvent> {
        self.events.iter().filter(|e| query.matches(e)).collect()
    }

    /// Drops every event that has aged out under `policy` as of `now`.
    /// Returns how many events were removed.
    pub fn purge(&mut self, policy: crate::retention::RetentionPolicy, now: u64) -> usize {
        let before = self.events.len();
        self.events
            .retain(|e| !policy.is_expired_at(e.timestamp, now));
        before - self.events.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TenantId;

    fn event(msg: u64, tenant: u32, rcpt: &str, status: DeliveryStatus, at: u64) -> DeliveryEvent {
        DeliveryEvent::new(msg, TenantId::from_index(tenant), rcpt, status, at)
    }

    #[test]
    fn given_a_recorded_event_when_queried_by_its_tenant_then_it_is_returned() {
        let acme = TenantId::from_index(0);
        let globex = TenantId::from_index(1);
        let mut history = DeliveryHistory::new();
        history.record(event(1, 0, "a@acme.example", DeliveryStatus::Delivered, 100));

        let acme_hits = history.query(&DeliveryQuery::for_tenant(acme));
        let globex_hits = history.query(&DeliveryQuery::for_tenant(globex));

        assert_eq!(acme_hits.len(), 1);
        assert_eq!(acme_hits[0].message_id(), 1);
        assert!(globex_hits.is_empty());
    }

    #[test]
    fn given_events_when_filtering_by_recipient_then_only_matching_ones_are_returned() {
        let mut history = DeliveryHistory::new();
        history.record(event(1, 0, "a@x.example", DeliveryStatus::Delivered, 10));
        history.record(event(2, 0, "b@x.example", DeliveryStatus::Delivered, 20));

        let hits = history.query(&DeliveryQuery::default().recipient("b@x.example"));

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id(), 2);
    }

    #[test]
    fn given_events_when_filtering_by_status_then_only_matching_ones_are_returned() {
        let mut history = DeliveryHistory::new();
        history.record(event(1, 0, "a@x.example", DeliveryStatus::Delivered, 10));
        history.record(event(2, 0, "a@x.example", DeliveryStatus::Bounced, 20));
        history.record(event(3, 0, "a@x.example", DeliveryStatus::Deferred, 30));

        let bounced = history.query(&DeliveryQuery::default().status(DeliveryStatus::Bounced));

        assert_eq!(bounced.len(), 1);
        assert_eq!(bounced[0].message_id(), 2);
    }

    #[test]
    fn given_events_when_filtering_by_time_range_then_only_events_inside_are_returned() {
        let mut history = DeliveryHistory::new();
        history.record(event(1, 0, "a@x.example", DeliveryStatus::Delivered, 10));
        history.record(event(2, 0, "a@x.example", DeliveryStatus::Delivered, 50));
        history.record(event(3, 0, "a@x.example", DeliveryStatus::Delivered, 90));

        // [since, until) half-open window.
        let hits = history.query(&DeliveryQuery::default().since(50).until(90));

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id(), 2);
    }

    #[test]
    fn given_multiple_criteria_when_querying_then_they_are_combined_with_and() {
        let acme = TenantId::from_index(0);
        let mut history = DeliveryHistory::new();
        history.record(event(1, 0, "a@x.example", DeliveryStatus::Bounced, 10));
        history.record(event(2, 0, "a@x.example", DeliveryStatus::Delivered, 20));
        history.record(event(3, 1, "a@x.example", DeliveryStatus::Bounced, 30));

        let hits = history.query(
            &DeliveryQuery::for_tenant(acme)
                .recipient("a@x.example")
                .status(DeliveryStatus::Bounced),
        );

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id(), 1);
    }

    #[test]
    fn given_a_retention_policy_when_purging_then_only_aged_out_events_are_removed() {
        use crate::retention::RetentionPolicy;

        let mut history = DeliveryHistory::new();
        history.record(event(1, 0, "a@x.example", DeliveryStatus::Delivered, 0));
        history.record(event(2, 0, "a@x.example", DeliveryStatus::Delivered, 5_000));

        // Retain for 3600s; evaluate at t=7200 -> event #1 (age 7200) purged,
        // event #2 (age 2200) kept.
        let removed = history.purge(RetentionPolicy::of_seconds(3600), 7200);

        assert_eq!(removed, 1);
        let remaining = history.query(&DeliveryQuery::default());
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].message_id(), 2);
    }
}
