/*
 * SPDX-FileCopyrightText: 2026 Stalwart contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 *
 * Data-retention policy: decide when a timestamped item has aged out.
 */

/// A retention window expressed in seconds. `None` means keep forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetentionPolicy {
    seconds: Option<u64>,
}

impl RetentionPolicy {
    /// Keep items forever.
    pub fn unlimited() -> Self {
        RetentionPolicy { seconds: None }
    }

    /// Keep items for `seconds` after their creation.
    pub fn of_seconds(seconds: u64) -> Self {
        RetentionPolicy {
            seconds: Some(seconds),
        }
    }

    /// Returns `true` if an item created at `created_at` (epoch seconds) has
    /// aged past this policy by `now` (epoch seconds).
    pub fn is_expired_at(&self, created_at: u64, now: u64) -> bool {
        match self.seconds {
            Some(retention) => now.saturating_sub(created_at) > retention,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_a_finite_policy_when_item_is_older_than_retention_then_it_is_expired() {
        let policy = RetentionPolicy::of_seconds(3600);

        // Created at t=0, now at t=7200 -> aged 2h, retention 1h.
        assert!(policy.is_expired_at(0, 7200));
    }

    #[test]
    fn given_a_finite_policy_when_item_is_within_retention_then_it_is_not_expired() {
        let policy = RetentionPolicy::of_seconds(3600);

        assert!(!policy.is_expired_at(0, 1800));
    }

    #[test]
    fn given_a_finite_policy_when_item_age_equals_retention_then_it_is_not_yet_expired() {
        let policy = RetentionPolicy::of_seconds(3600);

        // Exactly at the boundary: still retained (expiry is strictly after).
        assert!(!policy.is_expired_at(0, 3600));
    }

    #[test]
    fn given_an_unlimited_policy_when_item_is_ancient_then_it_is_never_expired() {
        let policy = RetentionPolicy::unlimited();

        assert!(!policy.is_expired_at(0, u64::MAX));
    }
}
