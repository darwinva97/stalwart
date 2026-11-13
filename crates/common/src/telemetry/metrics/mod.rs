/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use crate::{Inner, network::memory::MemoryClass};
use trc::{Collector, MetricType};

pub mod otel;
pub mod prometheus;

// [fragmento SEL eliminado: 6 líneas]

#[cfg(all(feature = "enterprise", any(feature = "dev_mode", stalwart_tests)))]
pub mod test_data;

impl Inner {
    pub fn refresh_live_gauges(&self) {
        let budget = &self.data.memory_budget;
        Collector::update_gauge(
            MetricType::ServerBufferedMemorySmtp,
            budget.used(MemoryClass::SmtpInbound),
        );
        Collector::update_gauge(
            MetricType::ServerBufferedMemoryRead,
            budget.used(MemoryClass::Read),
        );
        Collector::update_gauge(
            MetricType::CacheSwapPending,
            self.cache.swap.pending_targets(),
        );
    }
}
