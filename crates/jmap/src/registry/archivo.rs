/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! API del archivo de borrados (`x:ArchivedItem/get`, `/query`, `/set`) y recuperación de cuentas borradas.
//! Edición bezenti, fase 3. Especificación: `bezenti/especificaciones/archivo.md`.
//!
//! Quién ve qué (§6 de la especificación):
//!
//! - Un usuario: solo los elementos de su cuenta (el filtro `accountId` se ignora), como dice la documentación.
//! - El operador (permiso `impersonate`): los de cualquier cuenta, una cada vez con el filtro `accountId`.
//! - [bezenti] Un administrador de inquilino (`sysAccountGet` y un inquilino): los de las cuentas de su ámbito
//!   (su inquilino y sus descendientes) con el filtro `accountId`. Fuera del ámbito, nada: las consultas salen
//!   vacías y `get`/`set` responden `notFound`.

use crate::{
    api::query::QueryResponseBuilder,
    registry::{
        bezenti::TenantAccess,
        mapping::{RegistryGetResponse, RegistryQueryResponse, RegistrySetResponse},
        query::{RegistryQueryFilters, with_page_position},
    },
};
use common::{
    Server,
    auth::AccessToken,
    bezenti::{
        archivo::{ElementoArchivado, clave_item},
        inquilino::TenantScope,
    },
};
use compact_str::format_compact;
use jmap_proto::{error::set::SetError, types::state::State};
use jmap_tools::Key;
use registry::{
    jmap::{IntoValue, JmapValue},
    schema::{
        enums::{AccountType, ArchivedItemStatus, Permission, TaskAccountMaintenanceType},
        prelude::{Object, ObjectInner, ObjectType, Property},
        structs::{
            Account, GroupAccount, Task, TaskAccountMaintenance, TaskDestroyAccount,
            TaskRestoreArchivedItem, TaskStatus, UserAccount,
        },
    },
    types::{EnumImpl, datetime::UTCDateTime},
};
use std::str::FromStr;
use store::{
    registry::{
        RegistryQuery,
        write::{RegistryWrite, RegistryWriteResult},
    },
    write::BatchBuilder,
};
use trc::AddContext;
use types::id::Id;

/// Qué archivo puede ver quien hace la petición.
pub(crate) enum VistaArchivo {
    /// Operador: cualquier cuenta.
    Todo,
    /// Administrador de inquilino: su cuenta y las de su ámbito.
    Inquilino(TenantScope),
    /// Usuario: solo su cuenta.
    Propia,
}

impl VistaArchivo {
    pub fn new(access_token: &AccessToken, tenant: &TenantAccess) -> Self {
        if access_token.has_permission(Permission::Impersonate) && tenant.is_server_wide() {
            VistaArchivo::Todo
        } else if let Some(scope) = &tenant.scope
            && access_token.has_permission(Permission::SysAccountGet)
        {
            VistaArchivo::Inquilino(scope.clone())
        } else {
            VistaArchivo::Propia
        }
    }

    /// ¿Se respeta el filtro `accountId`?
    fn admite_filtro(&self) -> bool {
        !matches!(self, VistaArchivo::Propia)
    }

    pub async fn puede_ver(
        &self,
        server: &Server,
        propia: u32,
        account_id: u32,
    ) -> trc::Result<bool> {
        Ok(match self {
            VistaArchivo::Todo => true,
            VistaArchivo::Propia => account_id == propia,
            VistaArchivo::Inquilino(scope) => {
                account_id == propia
                    || server
                        .try_account(account_id)
                        .await?
                        .and_then(|account| account.tenant_id)
                        .is_some_and(|tenant_id| scope.contains(tenant_id))
            }
        })
    }
}

/// Lee un elemento visible para quien pregunta (`None` si no existe o no lo puede ver: para él no existe).
async fn elemento_visible(
    server: &Server,
    vista: &VistaArchivo,
    propia: u32,
    id: Id,
) -> trc::Result<Option<ElementoArchivado>> {
    match server.archived_item(id.id()).await? {
        Some(item) if vista.puede_ver(server, propia, item.account_id).await? => Ok(Some(item)),
        _ => Ok(None),
    }
}

pub(crate) async fn archived_item_get<'x>(
    mut get: RegistryGetResponse<'x>,
    access_token: &AccessToken,
) -> trc::Result<RegistryGetResponse<'x>> {
    let server = get.server;
    let vista = VistaArchivo::new(access_token, &get.tenant);
    let propia = get.account_id;
    // El `accountId` solo se quita de la respuesta a quien únicamente ve lo suyo.
    get.is_account_filtered = matches!(vista, VistaArchivo::Propia);

    let ids = match get.ids.take() {
        Some(ids) => ids,
        None => {
            server
                .registry()
                .query::<Vec<Id>>(
                    RegistryQuery::new(ObjectType::ArchivedItem)
                        .with_account(propia)
                        .with_limit(server.core.jmap.get_max_objects),
                )
                .await?
        }
    };

    for id in ids {
        match elemento_visible(server, &vista, propia, id).await? {
            Some(item) => {
                let mut objeto = item.a_objeto();
                // Fase 8: un guion Sieve archivado enseña su texto (`contents` del esquema).
                if let registry::schema::structs::ArchivedItem::SieveScript(guion) = &mut objeto
                    && let Some(texto) = server
                        .get_blob(item.blob_hash().as_slice(), 0..usize::MAX)
                        .await?
                {
                    guion.contents = String::from_utf8_lossy(&texto).into_owned();
                }
                get.insert(id, objeto.into_value())
            }
            None => get.not_found(id),
        }
    }

    Ok(get)
}

pub(crate) async fn archived_item_query(
    mut req: RegistryQueryResponse<'_>,
    tenant: &TenantAccess,
) -> trc::Result<QueryResponseBuilder> {
    let vista = VistaArchivo::new(req.access_token, tenant);
    let propia = req.request.account_id.document_id();
    let mut account_id = None;
    let mut filtro_invalido = false;

    req.request
        .extract_filters(|property, _, value| match property {
            Property::AccountId => {
                if vista.admite_filtro() {
                    match value.as_str().and_then(|s| Id::from_str(s).ok()) {
                        Some(id) => account_id = Some(id.document_id()),
                        None => filtro_invalido = true,
                    }
                }
                true
            }
            _ => false,
        })?;
    if filtro_invalido {
        return Err(trc::JmapEvent::UnsupportedFilter
            .into_err()
            .details("Invalid accountId filter"));
    }

    let account_id = account_id.unwrap_or(propia);
    let params = req
        .request
        .extract_parameters(req.server.core.jmap.query_max_results, Some(Property::Id))?;

    // Fuera del ámbito no hay nada (ni se cuenta).
    let (mut results, position) = if vista.puede_ver(req.server, propia, account_id).await? {
        let page = req
            .server
            .registry()
            .query_page::<Vec<Id>>(
                params.paged(RegistryQuery::new(ObjectType::ArchivedItem).with_account(account_id)),
            )
            .await?;
        let position = params.page_position(page.position)?;
        (page.ids, position)
    } else {
        (Vec::new(), None)
    };

    match params.sort_by {
        Property::Id => {
            if params.needs_reversal() {
                results.sort_unstable_by(|a, b| b.cmp(a));
            }
        }
        property => {
            return Err(trc::JmapEvent::UnsupportedSort
                .into_err()
                .details(format_compact!(
                    "Property {} is not supported for sorting",
                    property
                )));
        }
    }

    let mut response = with_page_position(
        QueryResponseBuilder::new(
            results.len(),
            req.server.core.jmap.query_max_results,
            State::Initial,
            &req.request,
        ),
        position,
    );
    for id in results {
        if !response.add_id(id) {
            break;
        }
    }
    Ok(response)
}

pub(crate) async fn archived_item_set(
    mut set: RegistrySetResponse<'_>,
) -> trc::Result<RegistrySetResponse<'_>> {
    let server = set.server;
    let vista = VistaArchivo::new(set.access_token, &set.tenant);
    let propia = set.account_id;

    set.fail_all_create("Archived items are created by the server when items are deleted");

    let mut batch = BatchBuilder::new();
    for (id, value) in std::mem::take(&mut set.update) {
        let Some(mut item) = elemento_visible(server, &vista, propia, id).await? else {
            set.response.not_updated.append(id, SetError::not_found());
            continue;
        };

        // Solo se puede cambiar `status` (pedir la restauración); el resto es el registro de lo que se borró.
        let mut status = None;
        let mut error = None;
        for (key, value) in value.into_expanded_object() {
            match (key, value) {
                (Key::Property(Property::Status), JmapValue::Str(value)) => {
                    match ArchivedItemStatus::parse(value.as_ref()) {
                        Some(value) => status = Some(value),
                        None => {
                            error = Some(
                                SetError::invalid_properties()
                                    .with_property(Property::Status)
                                    .with_description("Unknown status"),
                            )
                        }
                    }
                }
                (Key::Property(Property::Type), _) => {}
                (Key::Property(property), _) => {
                    error = Some(
                        SetError::invalid_properties()
                            .with_property(property)
                            .with_description("Only the status of an archived item can be changed"),
                    )
                }
                _ => {
                    error = Some(
                        SetError::invalid_properties()
                            .with_description("Only the status of an archived item can be changed"),
                    )
                }
            }
        }
        if let Some(error) = error {
            set.response.not_updated.append(id, error);
            continue;
        }

        if status == Some(ArchivedItemStatus::RequestRestore) && !item.restaurar {
            item.restaurar = true;
            batch
                .set(clave_item(id.id()), item.serializar())
                .schedule_task(Task::RestoreArchivedItem(TaskRestoreArchivedItem {
                    blob_id: item.blob_id(),
                    archived_item_type: item.tipo(),
                    account_id: Id::from(item.account_id),
                    created_at: UTCDateTime::from_timestamp(item.creado as i64),
                    archived_until: UTCDateTime::from_timestamp(item.hasta as i64),
                    status: TaskStatus::now(),
                    due: None,
                }))
                .commit_point();
        }
        set.response.updated.append(id, None);
    }

    for id in std::mem::take(&mut set.destroy) {
        match elemento_visible(server, &vista, propia, id).await? {
            Some(item) => {
                item.borrar(&mut batch, id.id());
                batch.commit_point();
                set.response.destroyed.push(id);
            }
            None => set.response.not_destroyed.append(id, SetError::not_found()),
        }
    }

    if !batch.is_empty() {
        server
            .commit_batch(batch)
            .await
            .caused_by(trc::location!())?;
    }

    Ok(set)
}

/// Estado de la tarea `DestroyAccount`: aplazada `archiveDeletedAccountsFor` si está configurado.
pub(crate) fn account_destruction_status(server: &Server) -> TaskStatus {
    match server.core.email.archive_deleted_accounts_for {
        Some(delay) => TaskStatus::at(UTCDateTime::now().timestamp() + delay as i64),
        None => TaskStatus::now(),
    }
}

/// Recupera una cuenta borrada cuya tarea `DestroyAccount` aún no se ha ejecutado (documentación pública,
/// «Restore a deleted account»): se vuelve a crear con su id, nombre, dominio y tipo originales; el resto
/// (credenciales, grupos, ajustes) queda por defecto. [bezenti] Vuelve a su inquilino: el del dominio.
pub(crate) async fn recover_account(
    server: &Server,
    task: &TaskDestroyAccount,
) -> trc::Result<Result<(), SetError<Property>>> {
    let tenant_id = server
        .domain_by_id(task.account_domain_id.document_id())
        .await?
        .and_then(|domain| domain.tenant_id)
        .map(Id::from);
    // Fase 8: la cuenta recuperada vuelve a contar para las cuotas de su inquilino (y de sus revendedores).
    if let Some(tenant_id) = tenant_id {
        let quota = match task.account_type {
            AccountType::User => registry::schema::enums::TenantStorageQuota::MaxAccounts,
            AccountType::Group => registry::schema::enums::TenantStorageQuota::MaxGroups,
        };
        if let Err(exceeded) = server
            .check_tenant_object_quota(tenant_id.document_id(), quota, 1)
            .await?
        {
            return Ok(Err(SetError::new(
                jmap_proto::error::set::SetErrorType::OverQuota,
            )
            .with_description(exceeded.description())));
        }
    }
    let account = match task.account_type {
        AccountType::User => Account::User(UserAccount {
            name: task.account_name.clone(),
            domain_id: task.account_domain_id,
            member_tenant_id: tenant_id,
            created_at: UTCDateTime::now(),
            ..Default::default()
        }),
        AccountType::Group => Account::Group(GroupAccount {
            name: task.account_name.clone(),
            domain_id: task.account_domain_id,
            member_tenant_id: tenant_id,
            created_at: UTCDateTime::now(),
            ..Default::default()
        }),
    };
    let object = Object {
        inner: ObjectInner::Account(Box::new(account)),
        revision: 0,
    };
    match server
        .registry()
        .write(RegistryWrite::insert_with_id(task.account_id, &object))
        .await
        .caused_by(trc::location!())?
    {
        RegistryWriteResult::Success(_) => {
            let mut batch = BatchBuilder::new();
            batch.schedule_task(Task::AccountMaintenance(TaskAccountMaintenance {
                account_id: task.account_id,
                maintenance_type: TaskAccountMaintenanceType::RebuildShares,
                status: TaskStatus::now(),
            }));
            server.commit_batch(batch).await?;
            let mut invalidator = common::cache::invalidate::CacheInvalidationBuilder::default();
            invalidator.process_create(&object);
            server.invalidate_caches(invalidator).await?;
            Ok(Ok(()))
        }
        _ => Ok(Err(SetError::forbidden().with_description(
            "Account recovery failed: the account could not be re-created (is its name in use?)",
        ))),
    }
}

/// Fase 8: ¿es esta tarea una cuenta borrada (tarea `DestroyAccount`) del ámbito del inquilino? El ámbito se mira
/// por el dominio de la cuenta (la cuenta ya no existe; al recuperarla vuelve al inquilino de su dominio).
pub(crate) async fn tenant_can_see_task(
    server: &Server,
    scope: &TenantScope,
    task: &Task,
) -> trc::Result<bool> {
    let Task::DestroyAccount(task) = task else {
        return Ok(false);
    };
    Ok(server
        .domain_by_id(task.account_domain_id.document_id())
        .await?
        .and_then(|domain| domain.tenant_id)
        .is_some_and(|tenant_id| scope.contains(tenant_id)))
}

/// Fase 8: las tareas `DestroyAccount` del ámbito del inquilino, `(vencimiento, id)` por orden de vencimiento.
pub(crate) async fn tenant_destroyed_accounts(
    server: &Server,
    scope: &TenantScope,
) -> trc::Result<Vec<(u64, u64)>> {
    use registry::schema::enums::TaskType;
    use store::{
        IterateParams, ValueKey,
        write::{TaskDue, TaskId, TaskQueueClass, ValueClass},
    };
    let mut due_ids = Vec::new();
    server
        .store()
        .iterate(
            IterateParams::new(
                ValueKey::from(ValueClass::TaskQueue(TaskQueueClass::Due {
                    id: TaskId::Assigned(0),
                    typ: TaskType::DestroyAccount,
                    due: 0,
                })),
                ValueKey::from(ValueClass::TaskQueue(TaskQueueClass::Due {
                    id: TaskId::Assigned(u64::MAX),
                    typ: TaskType::DestroyAccount,
                    due: u64::MAX,
                })),
            )
            .ascending()
            .no_values(),
            |key, _| {
                let TaskDue { due, id, typ } = TaskDue::deserialize(key)?;
                if typ == TaskType::DestroyAccount {
                    due_ids.push((due, id));
                }
                Ok(true)
            },
        )
        .await
        .caused_by(trc::location!())?;
    let mut visible = Vec::with_capacity(due_ids.len());
    let mut seen = store::ahash::AHashSet::new();
    for (due, id) in due_ids {
        if !seen.insert(id) {
            continue;
        }
        if let Some(task) = server
            .store()
            .get_value::<Task>(ValueKey::from(ValueClass::TaskQueue(
                TaskQueueClass::Task {
                    id: TaskId::Assigned(id),
                },
            )))
            .await
            .caused_by(trc::location!())?
            && tenant_can_see_task(server, scope, &task).await?
        {
            visible.push((due, id));
        }
    }
    Ok(visible)
}

/// Fase 8: `x:Task/query` de un usuario de inquilino: solo las cuentas borradas de su ámbito, por vencimiento.
pub(crate) async fn tenant_task_query(
    req: RegistryQueryResponse<'_>,
    scope: &TenantScope,
) -> trc::Result<QueryResponseBuilder> {
    let mut entries = tenant_destroyed_accounts(req.server, scope).await?;
    let descending = req
        .request
        .sort
        .as_ref()
        .and_then(|sort| sort.first())
        .is_some_and(|comp| !comp.is_ascending);
    if descending {
        entries.reverse();
    }
    let mut response = QueryResponseBuilder::new(
        entries.len(),
        req.server.core.jmap.query_max_results,
        State::Initial,
        &req.request,
    );
    for (_, id) in entries {
        if !response.add_id(Id::from(id)) {
            break;
        }
    }
    Ok(response)
}
