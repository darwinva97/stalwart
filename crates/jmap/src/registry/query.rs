/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use crate::{
    api::query::QueryResponseBuilder,
    registry::{
        EnterpriseRegistry,
        mapping::{
            RegistryQueryResponse, account::credential_query, cluster::cluster_node_query,
            index_queue::index_queue_status_query, log::log_query,
            queued_message::queued_message_query, report::report_query,
            spam_sample::spam_sample_query, task::task_query,
        },
    },
};
use common::{Server, auth::AccessToken};
use compact_str::format_compact;
use jmap_proto::{
    method::query::{Comparator, Filter, QueryRequest, QueryResponse},
    object::registry::{Registry, RegistryComparator, RegistryFilter, RegistryFilterOperator},
    types::state::State,
};
use registry::{
    schema::{
        enums::{AccountType, ExternalPublicKeyStatus, Permission},
        prelude::{ObjectType, Property},
    },
    types::{
        EnumImpl,
        index::{IndexSchemaType, IndexSchemaValueType},
        ipmask::IpAddrOrMask,
    },
};
use std::str::FromStr;
use store::registry::{RegistryFilterOp, RegistryFilterValue};
use types::id::Id;

pub trait RegistryQuery: Sync + Send {
    fn registry_query(
        &self,
        object_type: ObjectType,
        request: QueryRequest<Registry>,
        access_token: &AccessToken,
    ) -> impl Future<Output = trc::Result<QueryResponse>> + Send;
}

impl RegistryQuery for Server {
    async fn registry_query(
        &self,
        object_type: ObjectType,
        mut request: QueryRequest<Registry>,
        access_token: &AccessToken,
    ) -> trc::Result<QueryResponse> {
        // Initial assertions
        if self.registry().is_bootstrap_mode() {
            return Err(trc::JmapEvent::Forbidden.into_err().details(concat!(
                "The server is in bootstrap mode. Only the 'Bootstrap' object type ",
                "can be accessed until the bootstrap process is complete.",
            )));
        }
        self.assert_enterprise_object(object_type)?;
        // bezenti: ámbito de inquilino (y tipos de objeto prohibidos a los usuarios de un inquilino).
        #[cfg(feature = "bezenti")]
        let tenant =
            crate::registry::bezenti::TenantAccess::new(self, access_token, object_type).await?;

        match object_type {
            ObjectType::ArfExternalReport
            | ObjectType::DmarcExternalReport
            | ObjectType::TlsExternalReport
            | ObjectType::DmarcInternalReport
            | ObjectType::TlsInternalReport => report_query(
                RegistryQueryResponse {
                    server: self,
                    access_token,
                    object_type,
                    request,
                },
                #[cfg(feature = "bezenti")]
                &tenant,
            )
            .await
            .and_then(|response| response.build()),

            // [fragmento SEL eliminado: 35 líneas]
            ObjectType::SpamTrainingSample => spam_sample_query(RegistryQueryResponse {
                server: self,
                access_token,
                object_type,
                request,
            })
            .await
            .and_then(|response| response.build()),

            ObjectType::QueuedMessage => queued_message_query(
                RegistryQueryResponse {
                    server: self,
                    access_token,
                    object_type,
                    request,
                },
                #[cfg(feature = "bezenti")]
                &tenant,
            )
            .await
            .and_then(|response| response.build()),

            ObjectType::ClusterNode => cluster_node_query(RegistryQueryResponse {
                server: self,
                access_token,
                object_type,
                request,
            })
            .await
            .and_then(|response| response.build()),

            ObjectType::ApiKey | ObjectType::AppPassword | ObjectType::Passkey => {
                credential_query(RegistryQueryResponse {
                    server: self,
                    access_token,
                    object_type,
                    request,
                })
                .await
                .and_then(|response| response.build())
            }

            ObjectType::IndexQueueStatus => index_queue_status_query(RegistryQueryResponse {
                server: self,
                access_token,
                object_type,
                request,
            })
            .await
            .and_then(|response| response.build()),

            // bezenti (fase 8): un usuario de inquilino solo ve las cuentas borradas de su ámbito.
            #[cfg(feature = "bezenti")]
            ObjectType::Task if tenant.scope.is_some() => {
                crate::registry::archivo::tenant_task_query(
                    RegistryQueryResponse {
                        server: self,
                        access_token,
                        object_type,
                        request,
                    },
                    tenant.scope.as_ref().unwrap(),
                )
                .await
                .and_then(|response| response.build())
            }
            ObjectType::Task => task_query(RegistryQueryResponse {
                server: self,
                access_token,
                object_type,
                request,
            })
            .await
            .and_then(|response| response.build()),

            ObjectType::Log => log_query(RegistryQueryResponse {
                server: self,
                access_token,
                object_type,
                request,
            })
            .await
            .and_then(|response| response.build()),

            ObjectType::Action => Err(trc::JmapEvent::InvalidArguments
                .into_err()
                .details("Actions cannot be queried")),

            // bezenti: archivo de borrados (fase 3).
            #[cfg(feature = "bezenti")]
            ObjectType::ArchivedItem => crate::registry::archivo::archived_item_query(
                RegistryQueryResponse {
                    server: self,
                    access_token,
                    object_type,
                    request,
                },
                &tenant,
            )
            .await
            .and_then(|response| response.build()),

            // bezenti: historial de telemetría (fase 5).
            #[cfg(feature = "bezenti")]
            ObjectType::Trace => crate::registry::telemetria::trace_query(
                RegistryQueryResponse {
                    server: self,
                    access_token,
                    object_type,
                    request,
                },
                &tenant,
            )
            .await
            .and_then(|response| response.build()),
            #[cfg(feature = "bezenti")]
            ObjectType::Metric => crate::registry::telemetria::metric_query(
                RegistryQueryResponse {
                    server: self,
                    access_token,
                    object_type,
                    request,
                },
                &tenant,
            )
            .await
            .and_then(|response| response.build()),

            _ => {
                let mut query = store::registry::RegistryQuery::new(object_type);
                // [fragmento SEL eliminado: 8 líneas]
                // bezenti: solo el ámbito del inquilino (con un filtro si es un solo inquilino; si no, después).
                #[cfg(feature = "bezenti")]
                {
                    query = tenant.restrict_query(query);
                }
                #[cfg(feature = "bezenti")]
                let post_filter = if tenant.needs_post_filter(object_type) {
                    tenant.visible_ids(self, object_type).await?
                } else {
                    None
                };
                let can_impersonate = access_token.has_permission(Permission::Impersonate);
                let mut has_account_filter = false;
                let indexes = object_type.indexes();
                request.extract_filters(|property, op, value| match property {
                    // [fragmento SEL eliminado: 6 líneas]
                    Property::AccountId if !can_impersonate => true,
                    property => {
                        let Some(index) = indexes.iter().find(|i| i.prop == property) else {
                            return false;
                        };
                        let is_pk = index.typ == IndexSchemaType::Unique;

                        let value = match (index.value, value) {
                            (IndexSchemaValueType::Keyword, serde_json::Value::String(value)) => {
                                Some(RegistryFilterValue::from(value))
                            }
                            (IndexSchemaValueType::Text, serde_json::Value::String(value)) => {
                                query.push_text(property, value);
                                return true;
                            }
                            (IndexSchemaValueType::Number, serde_json::Value::Number(value)) => {
                                value
                                    .as_i64()
                                    .map(|value| RegistryFilterValue::from(value as u64))
                            }
                            (IndexSchemaValueType::Enum, serde_json::Value::String(value))
                                if (property == Property::Type
                                    && object_type == ObjectType::Account) =>
                            {
                                AccountType::parse(&value)
                                    .map(|id| RegistryFilterValue::from(id.to_id()))
                            }
                            (IndexSchemaValueType::Enum, serde_json::Value::String(value))
                                if (property == Property::Status
                                    && object_type == ObjectType::ExternalPublicKey) =>
                            {
                                ExternalPublicKeyStatus::parse(&value)
                                    .map(|id| RegistryFilterValue::from(id.to_id()))
                            }
                            (IndexSchemaValueType::Boolean, serde_json::Value::Bool(value)) => {
                                Some(RegistryFilterValue::from(value))
                            }
                            (IndexSchemaValueType::Id, serde_json::Value::String(value)) => {
                                Id::from_str(&value)
                                    .ok()
                                    .map(|id| RegistryFilterValue::from(id.id()))
                            }
                            (IndexSchemaValueType::IpMask, serde_json::Value::String(value)) => {
                                IpAddrOrMask::from_str(&value)
                                    .ok()
                                    .map(|ip| RegistryFilterValue::Bytes(ip.to_index_key()))
                            }
                            _ => None,
                        };

                        if let Some(value) = value {
                            has_account_filter |= property == Property::AccountId;
                            query.filters.push(store::registry::RegistryFilter {
                                property,
                                op,
                                value,
                                is_pk,
                            });

                            true
                        } else {
                            false
                        }
                    }
                })?;
                if !has_account_filter {
                    query = query.with_account(request.account_id.document_id());
                }

                // bezenti: con filtro posterior no se pagina en el almacén (se pagina sobre el resultado filtrado).
                #[cfg(feature = "bezenti")]
                let params = request.extract_parameters(
                    self.core.jmap.query_max_results,
                    post_filter.is_none().then_some(Property::Id),
                )?;
                #[cfg(not(feature = "bezenti"))]
                let params = request
                    .extract_parameters(self.core.jmap.query_max_results, Some(Property::Id))?;

                let mut position = None;
                let matches = if query.has_filters() || params.sort_by == Property::Id {
                    let page = self
                        .registry()
                        .query_page::<Vec<Id>>(params.paged(query))
                        .await?;
                    position = params.page_position(page.position)?;
                    if page.ids.is_empty() {
                        return with_page_position(
                            QueryResponseBuilder::new(
                                0,
                                self.core.jmap.query_max_results,
                                State::Initial,
                                &request,
                            ),
                            position,
                        )
                        .build();
                    }
                    page.ids.into()
                } else {
                    None
                };

                let results = match params.sort_by {
                    Property::Id => {
                        let mut results = matches.unwrap();
                        if params.needs_reversal() {
                            results.sort_unstable_by(|a, b| b.cmp(a));
                        }
                        results
                    }
                    property => {
                        let Some(index) = indexes
                            .iter()
                            .find(|i| i.prop == property && i.value != IndexSchemaValueType::Text)
                        else {
                            return Err(trc::JmapEvent::UnsupportedSort.into_err().details(
                                format_compact!(
                                    "Property {} is not supported for sorting",
                                    property
                                ),
                            ));
                        };

                        if index.typ == IndexSchemaType::Search {
                            self.registry()
                                .sort_by_index(
                                    object_type,
                                    index.prop,
                                    matches,
                                    params.sort_ascending,
                                )
                                .await?
                        } else {
                            self.registry()
                                .sort_by_pk(object_type, index.prop, matches, params.sort_ascending)
                                .await?
                        }
                    }
                };

                // bezenti: fuera lo que no es del ámbito, antes de contar y paginar.
                #[cfg(feature = "bezenti")]
                let results = match &post_filter {
                    Some(visible) => results
                        .into_iter()
                        .filter(|id| visible.contains(id.document_id()))
                        .collect::<Vec<_>>(),
                    None => results,
                };

                // Build response
                let mut response = with_page_position(
                    QueryResponseBuilder::new(
                        results.len(),
                        self.core.jmap.query_max_results,
                        State::Initial,
                        &request,
                    ),
                    position,
                );

                for id in results {
                    if !response.add_id(id) {
                        break;
                    }
                }

                response.build()
            }
        }
    }
}

pub(crate) trait RegistryQueryFilters {
    fn extract_filters(
        &mut self,
        cb: impl FnMut(Property, RegistryFilterOp, serde_json::Value) -> bool,
    ) -> trc::Result<()>;

    fn extract_parameters(
        &mut self,
        max_results: usize,
        external_filter: Option<Property>,
    ) -> trc::Result<RegistryQueryParameters>;
}

pub(crate) struct RegistryQueryParameters {
    pub sort_by: Property,
    pub sort_ascending: bool,
    pub anchor: Option<u64>,
    pub position: Option<u64>,
    pub limit: Option<usize>,
}

impl RegistryQueryParameters {
    pub fn paged(&self, query: store::registry::RegistryQuery) -> store::registry::RegistryQuery {
        let Some(limit) = self.limit else {
            return query;
        };
        let query = query.with_limit(limit);
        let query = if self.sort_ascending {
            query.with_ascending()
        } else {
            query.with_descending()
        };
        match (self.anchor, self.position) {
            (Some(anchor), _) => query.with_anchor(anchor),
            (None, Some(position)) => query.with_index_start(position),
            (None, None) => query,
        }
    }

    pub fn page_position(&self, position: Option<usize>) -> trc::Result<Option<usize>> {
        match (self.limit, position) {
            (None, _) => Ok(None),
            (Some(_), Some(position)) => Ok(Some(position)),
            (Some(_), None) => Err(trc::JmapEvent::AnchorNotFound.into_err()),
        }
    }

    pub fn needs_reversal(&self) -> bool {
        !self.sort_ascending && self.limit.is_none()
    }
}

pub(crate) fn with_page_position(
    response: QueryResponseBuilder,
    position: Option<usize>,
) -> QueryResponseBuilder {
    match position {
        Some(position) => response.with_position(position),
        None => response,
    }
}

impl RegistryQueryFilters for QueryRequest<Registry> {
    fn extract_filters(
        &mut self,
        mut cb: impl FnMut(Property, RegistryFilterOp, serde_json::Value) -> bool,
    ) -> trc::Result<()> {
        for cond in std::mem::take(&mut self.filter) {
            match cond {
                Filter::Property(cond) => match cond {
                    RegistryFilter::Property {
                        property,
                        operator,
                        value,
                    } => {
                        let operator = match operator {
                            RegistryFilterOperator::Equal => RegistryFilterOp::Equal,
                            RegistryFilterOperator::GreaterThan => RegistryFilterOp::GreaterThan,
                            RegistryFilterOperator::GreaterThanOrEqual => {
                                RegistryFilterOp::GreaterEqualThan
                            }
                            RegistryFilterOperator::LessThan => RegistryFilterOp::LowerThan,
                            RegistryFilterOperator::LessThanOrEqual => {
                                RegistryFilterOp::LowerEqualThan
                            }
                        };
                        if !cb(property, operator, value) {
                            return Err(trc::JmapEvent::UnsupportedFilter.into_err().details(
                                format_compact!(
                                    "Filter on property {} is not supported or invalid",
                                    property
                                ),
                            ));
                        }
                    }
                    RegistryFilter::_T(other) => {
                        return Err(trc::JmapEvent::UnsupportedFilter.into_err().details(other));
                    }
                },
                Filter::And | Filter::Close => {}
                Filter::Or | Filter::Not => {
                    return Err(trc::JmapEvent::UnsupportedFilter
                        .into_err()
                        .details("Only AND is supported in filters"));
                }
            }
        }

        Ok(())
    }

    fn extract_parameters(
        &mut self,
        max_results: usize,
        external_filter: Option<Property>,
    ) -> trc::Result<RegistryQueryParameters> {
        let comparator = self
            .sort
            .take()
            .unwrap_or_default()
            .into_iter()
            .next()
            .unwrap_or_else(|| Comparator::descending(RegistryComparator::Property(Property::Id)));

        match comparator.property {
            RegistryComparator::Property(property) => {
                if external_filter.is_some_and(|f| f == property)
                    && !self.calculate_total.unwrap_or(false)
                    && self.anchor_offset.is_none_or(|offset| offset == 0)
                    && self.position.is_none_or(|pos| pos >= 0)
                {
                    Ok(RegistryQueryParameters {
                        sort_by: property,
                        sort_ascending: comparator.is_ascending,
                        anchor: self.anchor.take().map(|anchor| anchor.id()),
                        position: self.position.take().map(|pos| pos as u64),
                        limit: self
                            .limit
                            .take()
                            .map(|limit| std::cmp::min(limit, max_results))
                            .unwrap_or(max_results)
                            .into(),
                    })
                } else {
                    Ok(RegistryQueryParameters {
                        sort_by: property,
                        sort_ascending: comparator.is_ascending,
                        anchor: None,
                        position: None,
                        limit: None,
                    })
                }
            }
            RegistryComparator::_T(other) => Err(trc::JmapEvent::UnsupportedSort
                .into_err()
                .details(format_compact!(
                    "Property {} is not supported for sorting",
                    other
                ))),
        }
    }
}
