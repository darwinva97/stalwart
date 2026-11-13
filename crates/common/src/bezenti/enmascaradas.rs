/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Direcciones enmascaradas (`x:MaskedEmail`). Edición bezenti, fase 3.
//!
//! Especificación: `bezenti/especificaciones/enmascaradas.md`. Aquí vive lo que necesitan a la vez la API
//! (generar y validar direcciones) y la recepción de correo (reconocer una dirección enmascarada en `RCPT TO` y
//! decidir si se entrega a su cuenta o se rechaza como destinatario desconocido).
//!
//! Formato de la dirección (§2 de la especificación): `<prefijo>.<código>@<dominio>`. El prefijo son 1 a 64
//! caracteres `a-z`, `0-9` y `_` (sin `_` inicial); el código, [`LONGITUD_CODIGO`] caracteres del alfabeto
//! [`ALFABETO_CODIGO`]. Como el prefijo no puede llevar puntos, una dirección enmascarada tiene exactamente un
//! punto en la parte local y se reconoce sin consultar nada; solo esas direcciones cuestan una búsqueda en el
//! almacén (por la clave única de `x:MaskedEmail.email`).

use crate::Server;
use registry::{
    schema::{
        prelude::{ObjectType, Property},
        structs::MaskedEmail,
    },
    types::datetime::UTCDateTime,
};
use store::rand::{self, RngExt};
use trc::AddContext;

/// Longitud del código que identifica la dirección.
pub const LONGITUD_CODIGO: usize = 10;
/// Alfabeto del código: minúsculas y cifras sin los caracteres que se confunden (`0`/`o`, `1`/`l`).
pub const ALFABETO_CODIGO: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
/// Longitud del prefijo aleatorio cuando el cliente no da uno (documentación pública: 16).
pub const LONGITUD_PREFIJO: usize = 16;
const ALFABETO_PREFIJO: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
/// Longitud máxima del prefijo (esquema y documentación pública).
pub const MAX_PREFIJO: usize = 64;

/// Qué hacer con un destinatario que es una dirección enmascarada.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestinoEnmascarada {
    /// Se entrega a la cuenta `account_id`, reescribiendo el destinatario del sobre a `direccion` (una dirección
    /// de la cuenta en el mismo dominio). La cabecera `To` no cambia: el usuario ve a qué dirección le escribieron.
    Entregar { account_id: u32, direccion: String },
    /// Existe pero está desactivada, caducada o su cuenta ya no tiene dirección en ese dominio: se rechaza igual
    /// que un destinatario que no existe.
    Rechazar,
}

/// Valida un prefijo dado por el cliente y lo devuelve en minúsculas.
pub fn validar_prefijo(prefijo: &str) -> Result<String, &'static str> {
    let prefijo = prefijo.to_ascii_lowercase();
    if prefijo.is_empty() || prefijo.len() > MAX_PREFIJO {
        Err("The prefix must be between 1 and 64 characters long")
    } else if prefijo.starts_with('_') {
        Err("The prefix cannot start with an underscore")
    } else if !prefijo
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        Err("The prefix may only contain the characters a-z, 0-9 and _")
    } else {
        Ok(prefijo)
    }
}

fn aleatorio(alfabeto: &[u8], longitud: usize) -> String {
    let mut rng = rand::rng();
    (0..longitud)
        .map(|_| alfabeto[rng.random_range(0..alfabeto.len())] as char)
        .collect()
}

/// Prefijo aleatorio (16 caracteres `a-z0-9`).
pub fn prefijo_aleatorio() -> String {
    aleatorio(ALFABETO_PREFIJO, LONGITUD_PREFIJO)
}

/// Código aleatorio de la dirección.
pub fn codigo_aleatorio() -> String {
    aleatorio(ALFABETO_CODIGO, LONGITUD_CODIGO)
}

/// Parte local `<prefijo>.<código>`.
pub fn parte_local(prefijo: &str, codigo: &str) -> String {
    format!("{prefijo}.{codigo}")
}

/// ¿Tiene esta parte local la forma de una dirección enmascarada? (sin consultar el almacén).
pub fn parece_enmascarada(local: &str) -> bool {
    let Some((prefijo, codigo)) = local.split_once('.') else {
        return false;
    };
    codigo.len() == LONGITUD_CODIGO
        && codigo.bytes().all(|b| ALFABETO_CODIGO.contains(&b))
        && !prefijo.is_empty()
        && prefijo.len() <= MAX_PREFIJO
        && !prefijo.starts_with('_')
        && prefijo
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// ¿Recibe correo ahora mismo? (activada y sin caducar).
pub fn esta_vigente(masked: &MaskedEmail, ahora: i64) -> bool {
    masked.enable
        && masked
            .expires_at
            .as_ref()
            .is_none_or(|expira| expira.timestamp() > ahora)
}

impl Server {
    /// Busca la dirección enmascarada `local@dominio` (en minúsculas). `None` si no hay ninguna.
    pub async fn masked_email_by_address(
        &self,
        address: &str,
    ) -> trc::Result<Option<(u64, MaskedEmail)>> {
        let Some(object_id) = self
            .registry()
            .primary_key(
                Some(ObjectType::MaskedEmail),
                Property::Email,
                address.as_bytes().to_vec(),
            )
            .await
            .caused_by(trc::location!())?
        else {
            return Ok(None);
        };
        let item_id = object_id.id().id();
        Ok(self
            .registry()
            .object::<MaskedEmail>(object_id.id())
            .await
            .caused_by(trc::location!())?
            .map(|masked| (item_id, masked)))
    }

    /// Resolución de un destinatario en `RCPT TO` (y en la entrega): `None` si la dirección no es enmascarada
    /// (se sigue con la resolución normal).
    pub async fn masked_email_resolve(
        &self,
        local_part: &str,
        domain_part: &str,
        domain_id: u32,
    ) -> trc::Result<Option<DestinoEnmascarada>> {
        let local_part = local_part.to_ascii_lowercase();
        if !parece_enmascarada(&local_part) {
            return Ok(None);
        }
        let domain_part = domain_part.to_ascii_lowercase();
        let Some((_, masked)) = self
            .masked_email_by_address(&format!("{local_part}@{domain_part}"))
            .await?
        else {
            return Ok(None);
        };

        if !esta_vigente(&masked, UTCDateTime::now().timestamp()) {
            return Ok(Some(DestinoEnmascarada::Rechazar));
        }

        // La cuenta debe seguir existiendo y tener una dirección en el dominio del destinatario.
        let account_id = masked.account_id.document_id();
        let Some(account) = self.try_account(account_id).await? else {
            return Ok(Some(DestinoEnmascarada::Rechazar));
        };
        Ok(Some(
            match account
                .addresses
                .iter()
                .find(|address| address.domain_id == domain_id)
            {
                Some(address) => DestinoEnmascarada::Entregar {
                    account_id,
                    direccion: format!("{}@{domain_part}", address.local_part),
                },
                None => DestinoEnmascarada::Rechazar,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formato_de_las_direcciones() {
        for _ in 0..100 {
            let local = parte_local(&prefijo_aleatorio(), &codigo_aleatorio());
            assert!(parece_enmascarada(&local), "{local}");
        }
        let local = parte_local("boletin_2026", &codigo_aleatorio());
        assert!(parece_enmascarada(&local));
        assert!(!parece_enmascarada("juan.perez"));
        assert!(!parece_enmascarada("juan"));
        assert!(!parece_enmascarada("a.b.abcdefghij"));
        assert!(!parece_enmascarada("_x.abcdefghij"));
        assert!(!parece_enmascarada(".abcdefghij"));
        assert!(!parece_enmascarada("x.abcdefghi0"));
    }

    #[test]
    fn validacion_del_prefijo() {
        assert_eq!(validar_prefijo("Boletin_1").unwrap(), "boletin_1");
        assert!(validar_prefijo("").is_err());
        assert!(validar_prefijo("_x").is_err());
        assert!(validar_prefijo("con.punto").is_err());
        assert!(validar_prefijo("con-guion").is_err());
        assert!(validar_prefijo(&"a".repeat(65)).is_err());
        assert!(validar_prefijo(&"a".repeat(64)).is_ok());
    }
}
