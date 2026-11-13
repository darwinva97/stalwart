/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::time::Duration;
use std::{
    collections::BinaryHeap,
    sync::Arc,
    time::{Instant, SystemTime},
};

use crate::task_manager::reconcile::insert_task_if_absent;
use common::{
    BuildServer, Inner, LONG_1D_SLUMBER,
    config::mailstore::spamfilter,
    gateway::store::{ExternalPublicKeyStore, KEY_CHECK_INTERVAL},
};
use registry::{
    schema::{
        enums::{TaskSpamFilterMaintenanceType, TaskStoreMaintenanceType, TaskType},
        structs::{
            Task, TaskExternalPublicKeyMaintenance, TaskSpamFilterMaintenance, TaskStatus,
            TaskStoreMaintenance,
        },
    },
    types::EnumImpl,
};
use store::write::now;
// [fragmento SEL eliminado: 6 líneas]
use trc::{ClusterEvent, Collector, MetricType, TaskManagerEvent, TelemetryEvent};

#[derive(PartialEq, Eq)]
struct Action {
    due: Instant,
    event: Event,
}

#[derive(PartialEq, Eq, Debug)]
enum Event {
    PurgeAccount,
    PurgeDataStore,
    PurgeBlobStore,
    OtelMetrics,
    CalculateMetrics,
    TrainSpamClassifier,
    CheckExternalPublicKeys,
    RenewNodeIdLease,
    // [fragmento SEL eliminado: 10 líneas]
}

#[derive(Default)]
struct Queue {
    heap: BinaryHeap<Action>,
}

// [fragmento SEL eliminado: 6 líneas]

pub fn spawn_task_scheduler(inner: Arc<Inner>) {
    tokio::spawn(async move {
        trc::event!(TaskManager(TaskManagerEvent::SchedulerStarted));
        let start_time = SystemTime::now();

        // Add all events to queue
        let mut queue = Queue::default();
        {
            let server = inner.build_server();

            // Account purge
            queue.schedule(
                Instant::now() + server.core.email.account_purge_frequency.time_to_next(),
                Event::PurgeAccount,
            );
            queue.schedule(
                Instant::now() + server.core.email.data_purge_frequency.time_to_next(),
                Event::PurgeDataStore,
            );
            queue.schedule(
                Instant::now() + server.core.email.blob_purge_frequency.time_to_next(),
                Event::PurgeBlobStore,
            );

            // Node ID lease renewal
            if server.core.storage.coordinator.is_enabled() {
                queue.schedule(
                    Instant::now() + server.registry().refresh_node_id_interval(),
                    Event::RenewNodeIdLease,
                );
            }

            // Spam classifier training
            if let Some(train_interval) = server
                .core
                .spam
                .classifier
                .as_ref()
                .and_then(|c| c.train_interval)
            {
                let next_train = match server.inner.data.spam_classifier.load().as_ref() {
                    spamfilter::SpamClassifier::FhClassifier {
                        last_trained_at, ..
                    }
                    | spamfilter::SpamClassifier::CcfhClassifier {
                        last_trained_at, ..
                    } => now().saturating_sub(*last_trained_at).min(train_interval),
                    spamfilter::SpamClassifier::Disabled => train_interval,
                };

                queue.schedule(
                    Instant::now() + Duration::from_secs(next_train),
                    Event::TrainSpamClassifier,
                );
            }

            // Refresh external keys
            queue.schedule(
                Instant::now() + KEY_CHECK_INTERVAL,
                Event::CheckExternalPublicKeys,
            );

            // OTEL Push Metrics
            if let Some(otel) = &server.core.metrics.otel {
                queue.schedule(Instant::now() + otel.interval, Event::OtelMetrics);
            }

            // Calculate expensive metrics
            queue.schedule(Instant::now(), Event::CalculateMetrics);

            // [fragmento SEL eliminado: 26 líneas]
        }

        // [fragmento SEL eliminado: 7 líneas]

        let mut next_metric_update = Instant::now();
        #[cfg(feature = "enterprise")]
        let mut license_retry = LICENSE_RETRY_MIN;

        loop {
            tokio::time::sleep(queue.wake_up_time()).await;

            let server = inner.build_server();
            let roles = &server.core.network.roles;
            let mut tasks = (roles.task_scheduler).then(Vec::new);

            while let Some(event) = queue.pop() {
                match event.event {
                    Event::PurgeAccount => {
                        queue.schedule(
                            Instant::now()
                                + server.core.email.account_purge_frequency.time_to_next(),
                            Event::PurgeAccount,
                        );

                        if let Some(tasks) = tasks.as_mut() {
                            trc::event!(
                                TaskManager(TaskManagerEvent::TaskQueued),
                                Type = TaskStoreMaintenanceType::PurgeAccounts.as_str()
                            );

                            tasks.push(Task::StoreMaintenance(TaskStoreMaintenance {
                                maintenance_type: TaskStoreMaintenanceType::PurgeAccounts,
                                status: TaskStatus::now(),
                                shard_index: None,
                            }));
                        }
                    }
                    Event::PurgeDataStore => {
                        queue.schedule(
                            Instant::now() + server.core.email.data_purge_frequency.time_to_next(),
                            Event::PurgeDataStore,
                        );

                        if let Some(tasks) = tasks.as_mut() {
                            trc::event!(
                                TaskManager(TaskManagerEvent::TaskQueued),
                                Type = TaskStoreMaintenanceType::PurgeData.as_str()
                            );

                            tasks.push(Task::StoreMaintenance(TaskStoreMaintenance {
                                maintenance_type: TaskStoreMaintenanceType::PurgeData,
                                status: TaskStatus::now(),
                                shard_index: None,
                            }));
                        }
                    }
                    Event::PurgeBlobStore => {
                        queue.schedule(
                            Instant::now() + server.core.email.blob_purge_frequency.time_to_next(),
                            Event::PurgeBlobStore,
                        );

                        if let Some(tasks) = tasks.as_mut() {
                            trc::event!(
                                TaskManager(TaskManagerEvent::TaskQueued),
                                Type = TaskStoreMaintenanceType::PurgeBlob.as_str()
                            );

                            tasks.push(Task::StoreMaintenance(TaskStoreMaintenance {
                                maintenance_type: TaskStoreMaintenanceType::PurgeBlob,
                                status: TaskStatus::now(),
                                shard_index: None,
                            }));
                        }
                    }
                    Event::RenewNodeIdLease => {
                        queue.schedule(
                            Instant::now() + server.registry().refresh_node_id_interval(),
                            Event::RenewNodeIdLease,
                        );

                        trc::event!(
                            Cluster(ClusterEvent::NodeIdRenewed),
                            NodeId = server.registry().node_id()
                        );

                        let server = server.clone();
                        tokio::spawn(async move {
                            if let Err(err) = server.registry().refresh_node_id_lease().await {
                                trc::error!(err.details("Failed to renew node ID lease"));
                            }
                        });
                    }
                    Event::OtelMetrics => {
                        if let Some(otel) = &server.core.metrics.otel {
                            queue.schedule(Instant::now() + otel.interval, Event::OtelMetrics);

                            if roles.metrics_push {
                                let otel = otel.clone();

                                // [fragmento SEL eliminado: 6 líneas]

                                #[cfg(not(feature = "enterprise"))]
                                let is_enterprise = false;

                                if is_enterprise {
                                    server.inner.refresh_live_gauges();
                                }

                                tokio::spawn(async move {
                                    let elapsed = Instant::now();
                                    otel.push_metrics(is_enterprise, start_time).await;

                                    trc::event!(
                                        Telemetry(TelemetryEvent::MetricsPushed),
                                        Elapsed = elapsed.elapsed()
                                    );
                                });
                            }
                        }
                    }
                    Event::CalculateMetrics => {
                        // Calculate expensive metrics every 5 minutes
                        queue.schedule(
                            Instant::now() + Duration::from_secs(5 * 60),
                            Event::CalculateMetrics,
                        );

                        let update_other_metrics = if Instant::now() >= next_metric_update {
                            next_metric_update = Instant::now() + Duration::from_secs(86400);
                            true
                        } else {
                            false
                        };

                        let server = server.clone();
                        tokio::spawn(async move {
                            let elapsed = Instant::now();
                            if server.core.network.roles.metrics_calculate {
                                // [fragmento SEL eliminado: 71 líneas]

                                if update_other_metrics {
                                    match server.total_accounts().await {
                                        Ok(total) => {
                                            Collector::update_gauge(
                                                MetricType::UserCount,
                                                total as u64,
                                            );
                                        }
                                        Err(err) => {
                                            trc::error!(
                                                err.details("Failed to obtain account count")
                                            );
                                        }
                                    }

                                    match server.total_domains().await {
                                        Ok(total) => {
                                            Collector::update_gauge(
                                                MetricType::DomainCount,
                                                total as u64,
                                            );
                                        }
                                        Err(err) => {
                                            trc::error!(
                                                err.details("Failed to obtain domain count")
                                            );
                                        }
                                    }
                                }
                            }

                            // [fragmento SEL eliminado: 20 líneas]

                            match tokio::task::spawn_blocking(memory_stats::memory_stats).await {
                                Ok(Some(stats)) => {
                                    Collector::update_gauge(
                                        MetricType::ServerMemory,
                                        stats.physical_mem as u64,
                                    );
                                }
                                Ok(None) => {}
                                Err(err) => {
                                    trc::error!(
                                        trc::EventType::Server(trc::ServerEvent::ThreadError,)
                                            .reason(err)
                                            .caused_by(trc::location!())
                                            .details("Join Error")
                                    );
                                }
                            }

                            trc::event!(
                                Telemetry(TelemetryEvent::MetricsCollected),
                                Elapsed = elapsed.elapsed()
                            );
                        });
                    }
                    Event::CheckExternalPublicKeys => {
                        queue.schedule(
                            Instant::now() + KEY_CHECK_INTERVAL,
                            Event::CheckExternalPublicKeys,
                        );

                        if server.core.email.gateway.is_some()
                            && let Some(tasks) = tasks.as_mut()
                        {
                            match server.has_due_external_public_keys(now()).await {
                                Ok(true) => {
                                    tasks.push(Task::ExternalPublicKeyMaintenance(
                                        TaskExternalPublicKeyMaintenance {
                                            status: TaskStatus::now(),
                                        },
                                    ));
                                }
                                Ok(false) => {}
                                Err(err) => {
                                    trc::error!(err.caused_by(trc::location!()));
                                }
                            }
                        }
                    }
                    Event::TrainSpamClassifier => {
                        if let Some(train_interval) = server
                            .core
                            .spam
                            .classifier
                            .as_ref()
                            .and_then(|c| c.train_interval)
                        {
                            // Schedule next training
                            queue.schedule(
                                Instant::now() + Duration::from_secs(train_interval),
                                Event::TrainSpamClassifier,
                            );

                            if let Some(tasks) = tasks.as_mut() {
                                trc::event!(
                                    TaskManager(TaskManagerEvent::TaskQueued),
                                    Type = TaskType::SpamFilterMaintenance.as_str()
                                );

                                tasks.push(Task::SpamFilterMaintenance(
                                    TaskSpamFilterMaintenance {
                                        maintenance_type: TaskSpamFilterMaintenanceType::Train,
                                        status: TaskStatus::now(),
                                    },
                                ));
                            }
                        }
                    }

                    // [fragmento SEL eliminado: 130 líneas]
                }
            }

            for task in tasks.into_iter().flatten() {
                if let Err(err) = insert_task_if_absent(&server, task).await {
                    trc::error!(err.details("Failed to write scheduled task"));
                }
            }
        }
    });
}

// [fragmento SEL eliminado: 28 líneas]

impl Queue {
    pub fn schedule(&mut self, due: Instant, event: Event) {
        trc::event!(
            TaskManager(TaskManagerEvent::TaskScheduled),
            Due = trc::Value::Timestamp(
                now() + due.saturating_duration_since(Instant::now()).as_secs()
            ),
            Name = event.name()
        );

        self.heap.push(Action { due, event });
    }

    pub fn wake_up_time(&self) -> Duration {
        self.heap
            .peek()
            .map(|e| e.due.saturating_duration_since(Instant::now()))
            .unwrap_or(LONG_1D_SLUMBER)
    }

    pub fn pop(&mut self) -> Option<Action> {
        if self.heap.peek()?.due <= Instant::now() {
            self.heap.pop()
        } else {
            None
        }
    }
}

impl Ord for Action {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.due.cmp(&other.due).reverse()
    }
}

impl PartialOrd for Action {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Event {
    fn name(&self) -> &'static str {
        match self {
            Event::PurgeAccount => "purgeAccount",
            Event::PurgeDataStore => "purgeDataStore",
            Event::PurgeBlobStore => "purgeBlobStore",
            Event::OtelMetrics => "otelMetrics",
            Event::CalculateMetrics => "calculateMetrics",
            Event::TrainSpamClassifier => "trainSpamClassifier",
            Event::CheckExternalPublicKeys => "checkExternalPublicKeys",
            Event::RenewNodeIdLease => "renewNodeIdLease",
            // [fragmento SEL eliminado: 10 líneas]
        }
    }
}
