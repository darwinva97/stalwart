/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! `x:MaskedEmail/set`: alta y cambios de una dirección enmascarada. Edición bezenti, fase 3.
//! Especificación: `bezenti/especificaciones/enmascaradas.md`, §3 y §4.
//!
//! `get` y `query` son los genéricos del registro (objeto de cuenta: cada usuario ve las suyas; el operador, con
//! el filtro `accountId`, las de cualquiera). Aquí solo se valida lo que el servidor pone o comprueba al crear:
//! la dirección (prefijo, código y dominio), el límite `maxMaskedAddresses`, `createdAt` y `createdBy`.

use crate::registry::mapping::{ObjectResponse, ValidationResult};
use common::{
    Server,
    auth::AccessToken,
    bezenti::enmascaradas::{codigo_aleatorio, parte_local, prefijo_aleatorio, validar_prefijo},
};
use jmap_proto::error::set::{SetError, SetErrorType};
use registry::{
    jmap::{IntoValue, JmapValue},
    schema::{
        enums::StorageQuota,
        prelude::{ObjectInner, ObjectType, Property},
        structs::{Account, Credential, MaskedEmail, OAuthGrant},
    },
    types::datetime::UTCDateTime,
};
use store::registry::{RegistryObjectCounter, RegistryQuery};
use trc::AddContext;
use types::id::Id;

pub(crate) async fn validate_masked_email(
    server: &Server,
    access_token: &AccessToken,
    masked: &mut MaskedEmail,
    old: Option<&MaskedEmail>,
) -> ValidationResult {
    let mut response = ObjectResponse::default();

    if let Some(old) = old {
        // `createdBy` lo pone el servidor; un cliente no lo puede cambiar después.
        masked.created_by = old.created_by.clone();
        return Ok(Ok(response));
    }

    let account_id = masked.account_id.document_id();
    let Some(account) = server.try_account(account_id).await? else {
        return Ok(Err(SetError::new(SetErrorType::InvalidForeignKey)
            .with_property(Property::AccountId)
            .with_description("The account does not exist")));
    };

    // Límite por cuenta: `quotas.maxMaskedAddresses` de la cuenta o, si no, `x:Email.maxMaskedAddresses` (5).
    // Las desactivadas cuentan hasta que se borran (documentación pública).
    let limit = server.object_quota(account.object_quotas(), StorageQuota::MaxMaskedAddresses);
    let used = server
        .registry()
        .query::<RegistryObjectCounter>(
            RegistryQuery::new(ObjectType::MaskedEmail).with_account(account_id),
        )
        .await?
        .0 as u32;
    if used >= limit {
        return Ok(Err(SetError::over_quota().with_description(format!(
            "You have exceeded your quota of {limit} masked addresses."
        ))));
    }

    // Prefijo.
    let prefijo = match masked.email_prefix.as_deref() {
        Some(prefijo) => match validar_prefijo(prefijo) {
            Ok(prefijo) => {
                masked.email_prefix = Some(prefijo.clone());
                prefijo
            }
            Err(reason) => {
                return Ok(Err(SetError::invalid_properties()
                    .with_property(Property::EmailPrefix)
                    .with_description(reason)));
            }
        },
        None => prefijo_aleatorio(),
    };

    // Dominio: uno en el que la cuenta tenga dirección; por defecto, el del nombre de la cuenta.
    let domain_name = match masked.email_domain.as_deref() {
        Some(name) => {
            let name = name.trim().to_ascii_lowercase();
            match server.domain(&name).await? {
                Some(domain)
                    if account
                        .addresses
                        .iter()
                        .any(|address| address.domain_id == domain.id) =>
                {
                    masked.email_domain = Some(name.clone());
                    name
                }
                _ => {
                    return Ok(Err(SetError::forbidden()
                        .with_property(Property::EmailDomain)
                        .with_description(
                            "The specified domain is not valid for this account.",
                        )));
                }
            }
        }
        None => match account.addresses.first() {
            Some(address) => match server.domain_by_id(address.domain_id).await? {
                Some(domain) if !domain.names.is_empty() => domain.names[0].to_string(),
                _ => {
                    return Ok(Err(SetError::forbidden()
                        .with_property(Property::EmailDomain)
                        .with_description(
                            "No valid domain is available for this account.",
                        )));
                }
            },
            None => {
                return Ok(Err(SetError::forbidden()
                    .with_property(Property::EmailDomain)
                    .with_description(
                        "No valid domain is available for this account.",
                    )));
            }
        },
    };

    // Caducidad: solo al crear, y en el futuro.
    let now = UTCDateTime::now();
    if let Some(expires_at) = &masked.expires_at
        && expires_at.timestamp() <= now.timestamp()
    {
        return Ok(Err(SetError::invalid_properties()
            .with_property(Property::ExpiresAt)
            .with_description("The expiration date must be in the future")));
    }

    // La dirección: `<prefijo>.<código>@<dominio>`, sin chocar con otra enmascarada ni con una dirección real.
    let mut email = None;
    for _ in 0..8 {
        let candidate = format!(
            "{}@{domain_name}",
            parte_local(&prefijo, &codigo_aleatorio())
        );
        if server.masked_email_by_address(&candidate).await?.is_none()
            && server.rcpt_id_from_email(&candidate).await?.is_none()
        {
            email = Some(candidate);
            break;
        }
    }
    let Some(email) = email else {
        return Ok(Err(SetError::forbidden().with_description(
            "Could not generate a unique masked address, try again",
        )));
    };

    masked.email = email;
    masked.created_at = now;
    masked.created_by = created_by(server, access_token).await?;

    response
        .object
        .insert_unchecked(Property::Email, JmapValue::Str(masked.email.clone().into()));
    response
        .object
        .insert_unchecked(Property::CreatedAt, masked.created_at.into_value());
    response
        .object
        .insert_unchecked(Property::CreatedBy, masked.created_by.clone().into_value());
    Ok(Ok(response))
}

/// Nombre del cliente que crea la dirección, a partir de las credenciales de la petición: el del cliente OAuth
/// o la descripción de la contraseña de aplicación / clave API. Con la contraseña principal, ninguno.
async fn created_by(server: &Server, access_token: &AccessToken) -> trc::Result<Option<String>> {
    if let Some(grant_id) = access_token.grant_id() {
        return Ok(server
            .registry()
            .object::<OAuthGrant>(Id::new(grant_id))
            .await
            .caused_by(trc::location!())?
            .map(|grant| grant.client_name.unwrap_or(grant.client_id))
            .filter(|name| !name.is_empty()));
    }
    if access_token.is_secondary_credential()
        && let Some(credential_id) = access_token.credential_id()
        && let Some(object) = server
            .registry()
            .get(registry::types::id::ObjectId::new(
                ObjectType::Account,
                Id::from(access_token.account_id()),
            ))
            .await
            .caused_by(trc::location!())?
        && let ObjectInner::Account(account) = object.inner
        && let Account::User(user) = *account
    {
        return Ok(user
            .credentials
            .values()
            .find_map(|credential| match credential {
                Credential::AppPassword(c) | Credential::ApiKey(c)
                    if c.credential_id.document_id() == credential_id
                        && !c.description.is_empty() =>
                {
                    Some(c.description.clone())
                }
                _ => None,
            }));
    }
    Ok(None)
}
