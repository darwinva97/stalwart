/*
 * SPDX-FileCopyrightText: 2026 Stalwart contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 *
 * Horizontal scalability: a deterministic shard router and a read/write
 * replica-routing policy. Pure logic, independent of any storage backend.
 */

use std::hash::{Hash, Hasher};

/// Identifies one shard within a [`ShardRouter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShardId(u32);

impl ShardId {
    pub fn index(self) -> u32 {
        self.0
    }
}

/// Maps arbitrary keys onto a fixed number of shards by hashing.
#[derive(Debug, Clone)]
pub struct ShardRouter {
    shard_count: u32,
}

impl ShardRouter {
    /// Creates a router over `shard_count` shards. A count of zero is treated
    /// as a single shard so routing never divides by zero.
    pub fn new(shard_count: u32) -> Self {
        ShardRouter {
            shard_count: shard_count.max(1),
        }
    }

    pub fn shard_count(&self) -> u32 {
        self.shard_count
    }

    /// Deterministically resolves `key` to a shard.
    pub fn shard_for(&self, key: u64) -> ShardId {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        ShardId((hasher.finish() % self.shard_count as u64) as u32)
    }
}

/// A primary backend plus zero or more read replicas, holding the actual
/// backend handles of type `H` (e.g. a store connection).
///
/// Reads try one replica (round-robin) and fall back to the primary on a
/// non-fatal error; a fatal error (e.g. an assertion failure) short-circuits.
/// Writes target the primary.
pub struct ReplicaSet<H> {
    primary: H,
    replicas: Vec<H>,
    next: std::sync::atomic::AtomicUsize,
}

impl<H> ReplicaSet<H> {
    pub fn new(primary: H, replicas: Vec<H>) -> Self {
        ReplicaSet {
            primary,
            replicas,
            next: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The write/authoritative backend.
    pub fn primary(&self) -> &H {
        &self.primary
    }

    /// Runs a read `op`, preferring a round-robin replica and falling back to
    /// the primary on a non-fatal error. `is_fatal` errors are returned as-is
    /// without touching the primary.
    pub fn read<T, E>(
        &self,
        op: impl Fn(&H) -> Result<T, E>,
        is_fatal: impl Fn(&E) -> bool,
    ) -> Result<T, E> {
        if !self.replicas.is_empty() {
            let idx = self
                .next
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                % self.replicas.len();
            match op(&self.replicas[idx]) {
                Ok(value) => return Ok(value),
                Err(err) if is_fatal(&err) => return Err(err),
                Err(_) => {}
            }
        }
        op(&self.primary)
    }
}

/// Whether an operation reads or writes, used to pick an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// A resolved routing target within a replica set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// The read/write primary.
    Primary,
    /// A read-only replica, identified by its index.
    Replica(usize),
}

/// A primary with zero or more read replicas. Writes always hit the primary;
/// reads are spread across replicas round-robin, falling back to the primary
/// when no replicas exist.
#[derive(Debug, Clone)]
pub struct ReplicaPool {
    replica_count: usize,
    next: usize,
}

impl ReplicaPool {
    pub fn new(replica_count: usize) -> Self {
        ReplicaPool {
            replica_count,
            next: 0,
        }
    }

    /// Resolves the endpoint for the next operation of the given `access`.
    pub fn route(&mut self, access: Access) -> Endpoint {
        match access {
            Access::Write => Endpoint::Primary,
            Access::Read if self.replica_count == 0 => Endpoint::Primary,
            Access::Read => {
                let idx = self.next % self.replica_count;
                self.next = (self.next + 1) % self.replica_count;
                Endpoint::Replica(idx)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_a_key_when_routed_twice_then_it_maps_to_the_same_shard() {
        let router = ShardRouter::new(8);

        let first = router.shard_for(1234);
        let second = router.shard_for(1234);

        assert_eq!(first, second);
        assert!(first.index() < 8);
    }

    #[test]
    fn given_a_single_shard_when_routing_any_key_then_it_maps_to_shard_zero() {
        let router = ShardRouter::new(1);

        for key in 0..50 {
            assert_eq!(router.shard_for(key).index(), 0);
        }
    }

    #[test]
    fn given_many_keys_when_routed_then_more_than_one_shard_is_used() {
        let router = ShardRouter::new(8);

        let used: std::collections::HashSet<u32> =
            (0..1000).map(|k| router.shard_for(k).index()).collect();

        assert!(used.len() > 1, "hashing should spread keys across shards");
    }

    #[test]
    fn given_a_write_when_routed_then_it_always_targets_the_primary() {
        let mut pool = ReplicaPool::new(3);

        assert_eq!(pool.route(Access::Write), Endpoint::Primary);
        assert_eq!(pool.route(Access::Write), Endpoint::Primary);
    }

    #[test]
    fn given_no_replicas_when_reading_then_it_falls_back_to_the_primary() {
        let mut pool = ReplicaPool::new(0);

        assert_eq!(pool.route(Access::Read), Endpoint::Primary);
    }

    #[test]
    fn given_replicas_when_reading_repeatedly_then_it_round_robins_across_them() {
        let mut pool = ReplicaPool::new(3);

        let picks: Vec<Endpoint> = (0..4).map(|_| pool.route(Access::Read)).collect();

        assert_eq!(
            picks,
            vec![
                Endpoint::Replica(0),
                Endpoint::Replica(1),
                Endpoint::Replica(2),
                Endpoint::Replica(0),
            ]
        );
    }

    #[test]
    fn given_no_replicas_when_running_a_read_then_it_uses_the_primary() {
        let set = ReplicaSet::new("primary", Vec::<&str>::new());

        let used: Result<&str, ()> = set.read(|h| Ok(*h), |_| false);

        assert_eq!(used, Ok("primary"));
    }

    #[test]
    fn given_a_healthy_replica_when_running_a_read_then_it_uses_the_replica() {
        let set = ReplicaSet::new("primary", vec!["r0"]);

        let used: Result<&str, ()> = set.read(|h| Ok(*h), |_| false);

        assert_eq!(used, Ok("r0"));
    }

    #[test]
    fn given_a_replica_that_fails_nonfatally_when_reading_then_it_falls_back_to_primary() {
        let set = ReplicaSet::new("primary", vec!["r0"]);

        let used = set.read(
            |h| if *h == "r0" { Err(1) } else { Ok(*h) },
            |_e| false,
        );

        assert_eq!(used, Ok("primary"));
    }

    #[test]
    fn given_a_replica_that_fails_fatally_when_reading_then_the_error_short_circuits() {
        let set = ReplicaSet::new("primary", vec!["r0"]);

        let used: Result<&str, i32> = set.read(
            |h| if *h == "r0" { Err(9) } else { Ok(*h) },
            |e| *e == 9,
        );

        assert_eq!(used, Err(9));
    }

    #[test]
    fn given_several_replicas_when_reading_repeatedly_then_reads_round_robin() {
        let set = ReplicaSet::new("primary", vec!["r0", "r1", "r2"]);

        let picks: Vec<&str> = (0..4)
            .map(|_| set.read(|h| Ok::<_, ()>(*h), |_| false).unwrap())
            .collect();

        assert_eq!(picks, vec!["r0", "r1", "r2", "r0"]);
    }

    #[test]
    fn given_a_replica_set_when_writing_then_the_primary_is_targeted() {
        let set = ReplicaSet::new("primary", vec!["r0"]);

        assert_eq!(*set.primary(), "primary");
    }
}
