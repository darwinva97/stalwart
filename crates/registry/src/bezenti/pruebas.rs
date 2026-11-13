/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Pruebas del modelo de datos de pago (fase 1): ida y vuelta JSON (serde), formato binario (pickle) y
//! conversión a valores JMAP. Los ejemplos se construyen a partir de resources/schema/schema.json.gz.

use crate::{
    jmap::{IntoValue, JmapValue},
    pickle::{Pickle, PickledStream},
    schema::prelude::*,
    types::ObjectImpl,
};
use std::fmt::Debug;

/// JSON → objeto → JSON → objeto: los dos objetos deben ser iguales y el JSON estable.
fn json_ida_y_vuelta<T>(json: &str) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + Debug,
{
    let obj: T = serde_json::from_str(json).unwrap_or_else(|e| panic!("JSON no válido: {e}\n{json}"));
    let texto = serde_json::to_string(&obj).unwrap();
    let otra: T = serde_json::from_str(&texto).unwrap();
    assert_eq!(obj, otra, "ida y vuelta JSON: {texto}");
    assert_eq!(texto, serde_json::to_string(&otra).unwrap());
    obj
}

/// Objeto → binario → objeto.
fn pickle_ida_y_vuelta<T>(obj: &T)
where
    T: ObjectImpl + Pickle + PartialEq + Debug,
{
    let bytes = obj.to_pickled_vec();
    let mut stream = PickledStream::new(&bytes).expect("cabecera");
    let otra = T::unpickle(&mut stream).expect("unpickle");
    assert_eq!(obj, &otra, "ida y vuelta binaria");
}

/// Claves del objeto JMAP que produce `into_value`.
fn claves(v: JmapValue<'static>) -> Vec<String> {
    let mut res = Vec::new();
    if let JmapValue::Object(map) = v {
        for (k, _) in map.into_vec() {
            res.push(k.to_string().into_owned());
        }
    }
    res
}

fn tiene(v: &[String], clave: &str) -> bool {
    v.iter().any(|k| k == clave)
}

#[test]
fn tipos_y_propiedades_nuevos() {
    for nombre in [
        "AiModel", "Alert", "ArchivedItem", "MaskedEmail", "Metric", "MetricsStore", "SpamAi", "Tenant",
        "Trace", "TracingStore",
    ] {
        let t = ObjectType::parse(nombre).unwrap_or_else(|| panic!("falta ObjectType::{nombre}"));
        assert_eq!(t.as_str(), nombre);
        assert_eq!(ObjectType::from_id(t.to_id()), Some(t));
    }
    // x:Enterprise (licencias) no se genera.
    assert_eq!(ObjectType::parse("Enterprise"), None);
    assert_eq!(Permission::parse("sysEnterpriseGet"), None);
    for p in ["memberTenantId", "externalId", "maxMaskedAddresses", "readReplicas", "urlFailover"] {
        let prop = Property::parse(p).unwrap_or_else(|| panic!("falta Property {p}"));
        assert_eq!(Property::from_id(prop.to_id()), Some(prop));
    }
    assert!(Permission::parse("sysTenantCreate").is_some());
    assert!(ObjectType::Tenant.flags() & OBJ_SEQ_ID != 0);
    assert!(ObjectType::Domain.flags() & OBJ_FILTER_TENANT != 0);
}

#[test]
fn inquilino() {
    let t: Tenant = json_ida_y_vuelta(
        r#"{"name":"acme","logo":"https://acme.example/logo.png",
            "quotas":{"maxAccounts":100,"maxDomains":5,"maxDiskQuota":1073741824},
            "permissions":{"@type":"Inherit"},"roles":{"@type":"Default"}}"#,
    );
    assert_eq!(t.name, "acme");
    assert_eq!(t.quotas.get(&TenantStorageQuota::MaxAccounts), Some(&100));
    let mut errores = Vec::new();
    assert!(t.validate(&mut errores), "{errores:?}");
    pickle_ida_y_vuelta(&t);
    let c = claves(t.into_value());
    assert!(tiene(&c, "name") && tiene(&c, "quotas") && tiene(&c, "usedDiskQuota"));
    // Nombre obligatorio.
    let mut errores = Vec::new();
    assert!(!Tenant::default().validate(&mut errores));
}

#[test]
fn direccion_enmascarada() {
    let mut m: MaskedEmail = json_ida_y_vuelta(
        r#"{"email":"x7k2@acme.example","forDomain":"tienda.example","description":"compras",
            "emailPrefix":"x7","enable":true}"#,
    );
    assert!(m.enable);
    m.account_id = Id::new(7);
    let otra: MaskedEmail = json_ida_y_vuelta(&serde_json::to_string(&m).unwrap());
    assert_eq!(otra.account_id, Id::new(7));
    // El valor por defecto del esquema: enable = true.
    assert!(MaskedEmail::default().enable);
    pickle_ida_y_vuelta(&m);
    assert!(tiene(&claves(m.into_value()), "forDomain"));
}

#[test]
fn elemento_archivado() {
    let a: ArchivedItem = json_ida_y_vuelta(
        r#"{"@type":"Email","from":"ana@example.org","subject":"Factura","size":2048,
            "status":"requestRestore"}"#,
    );
    match &a {
        ArchivedItem::Email(e) => {
            assert_eq!(e.subject, "Factura");
            assert_eq!(e.status, ArchivedItemStatus::RequestRestore);
        }
        otro => panic!("variante inesperada: {otro:?}"),
    }
    assert_eq!(a.object_type(), ArchivedItemType::Email);
    let s: ArchivedItem = json_ida_y_vuelta(r#"{"@type":"SieveScript","name":"filtro"}"#);
    assert_eq!(s.object_type(), ArchivedItemType::SieveScript);
    let v = a.into_value();
    assert!(tiene(&claves(v), "@type"));
    // ArchivedItem no se guarda en el registro (OBJ_NOT_STORED): no tiene formato binario.
    assert!(ArchivedItem::FLAGS & OBJ_NOT_STORED != 0);
}

#[test]
fn ia_antispam() {
    let d: SpamAi = json_ida_y_vuelta(r#"{"@type":"Disable"}"#);
    assert_eq!(d, SpamAi::Disable);
    let e: SpamAi = json_ida_y_vuelta(r#"{"@type":"Enable","mode":"Shadow","explanation":true}"#);
    match &e {
        SpamAi::Enable(p) => {
            assert_eq!(p.mode, SpamAiMode::Shadow);
            assert!(p.explanation);
            // Valores por defecto del esquema: categorías, señales e instrucciones.
            assert!(p.categories.len() >= 2);
            assert!(!p.signals.is_empty());
            assert!(!p.instructions.is_empty());
            assert!(!p.condition.else_.is_empty());
        }
        otro => panic!("variante inesperada: {otro:?}"),
    }
    pickle_ida_y_vuelta(&e);
    pickle_ida_y_vuelta(&SpamAi::Enable(Default::default()));
    // La condición tiene su contexto de expresión (variables de SPAM_AI_VARIABLE).
    if let SpamAi::Enable(p) = &e {
        let ctx = p.expression_ctxs();
        assert_eq!(ctx.len(), 1);
        assert_eq!(ctx[0].property, Property::Condition);
        assert!(std::ptr::eq(ctx[0].allowed_variables, SPAM_AI_VARIABLE));
    }
}

#[test]
fn modelos_de_ia() {
    let m: AiModel = json_ida_y_vuelta(
        r#"{"@type":"OpenAi","name":"gpt","model":"gpt-x","url":"https://api.example/v1",
            "profile":"Azure","reasoning":"Low","logprobs":true}"#,
    );
    match &m {
        AiModel::OpenAi(o) => {
            assert_eq!(o.profile, AiOpenAiProfile::Azure);
            assert_eq!(o.reasoning, AiReasoning::Low);
            assert!(o.logprobs);
        }
        otro => panic!("variante inesperada: {otro:?}"),
    }
    let mut errores = Vec::new();
    assert!(m.validate(&mut errores), "{errores:?}");
    pickle_ida_y_vuelta(&m);
    for tipo in ["Anthropic", "Gemini", "Ollama", "TypeSafe"] {
        let a: AiModel = json_ida_y_vuelta(&format!(r#"{{"@type":"{tipo}","name":"m","model":"x"}}"#));
        assert_eq!(a.object_type().as_str(), tipo);
        pickle_ida_y_vuelta(&a);
    }
    // La clave de API se oculta al leer.
    let mut o = AiModelOpenAi::default();
    o.name = "gpt".into();
    let c = claves(AiModel::OpenAi(o).into_value());
    assert!(tiene(&c, "apiKey"));
}

#[test]
fn alertas() {
    let a: Alert = json_ida_y_vuelta(
        r#"{"enable":true,"condition":{"else":"false","match":{"0":{"if":"x > 1","then":"true"}}},
            "emailAlert":{"@type":"Enabled","fromAddress":"alertas@example.org",
                          "to":{"admin@example.org":true},"subject":"Alerta","body":"Texto"},
            "eventAlert":{"@type":"Disabled"}}"#,
    );
    assert!(a.enable);
    assert_eq!(a.condition.match_.len(), 1);
    match &a.email_alert {
        AlertEmail::Enabled(p) => assert_eq!(p.subject, "Alerta"),
        otro => panic!("variante inesperada: {otro:?}"),
    }
    let mut errores = Vec::new();
    assert!(a.validate(&mut errores), "{errores:?}");
    pickle_ida_y_vuelta(&a);
    assert_eq!(a.expression_ctxs().len(), 1);
}

#[test]
fn campos_de_pago_en_objetos_libres() {
    // Dominio con inquilino, directorio y logo.
    let mut d: Domain = json_ida_y_vuelta(r#"{"name":"example.org","logo":"data:image/png;base64,AA=="}"#);
    d.member_tenant_id = Some(Id::new(3));
    d.directory_id = Some(Id::new(9));
    d.allow_scim_provisioning = true;
    let d: Domain = json_ida_y_vuelta(&serde_json::to_string(&d).unwrap());
    assert_eq!(d.member_tenant_id, Some(Id::new(3)));
    assert_eq!(d.directory_id, Some(Id::new(9)));
    assert!(d.allow_scim_provisioning);
    pickle_ida_y_vuelta(&d);
    let c = claves(d.clone().into_value());
    assert!(tiene(&c, "memberTenantId") && tiene(&c, "logo") && tiene(&c, "allowScimProvisioning"));
    // El índice incluye la clave ajena del inquilino.
    let mut i = IndexBuilder::default();
    d.index(&mut i);
    assert!(!i.is_empty());
    assert!(ObjectType::Account.indexes().iter().any(|x| x.prop == Property::MemberTenantId));
    assert!(ObjectType::Account.indexes().iter().any(|x| x.prop == Property::ExternalId));

    // Cuenta de usuario con inquilino e identificador externo (SCIM).
    let mut u = UserAccount::default();
    u.name = "ana".into();
    u.member_tenant_id = Some(Id::new(3));
    u.external_id = Some("00u1abcd".into());
    let cuenta = Account::User(u);
    let texto = serde_json::to_string(&cuenta).unwrap();
    assert!(texto.contains("memberTenantId") && texto.contains("externalId"));
    let cuenta: Account = json_ida_y_vuelta(&texto);
    pickle_ida_y_vuelta(&cuenta);

    // Campos de pago obligatorios (van en su propio bloque binario).
    let mut l = LdapDirectory::default();
    l.url_failover = Map::new(vec!["ldap://respaldo:389".to_string()]);
    l.member_tenant_id = Some(Id::new(2));
    let dir = Directory::Ldap(l);
    pickle_ida_y_vuelta(&dir);
    let dir: Directory = json_ida_y_vuelta(&serde_json::to_string(&dir).unwrap());
    if let Directory::Ldap(l) = &dir {
        assert_eq!(l.url_failover.len(), 1);
    }
    let mut c = Cache::default();
    c.tenants = 4096;
    pickle_ida_y_vuelta(&c);
    let c: Cache = json_ida_y_vuelta(&serde_json::to_string(&c).unwrap());
    assert_eq!(c.tenants, 4096);

    // Límite de direcciones enmascaradas en x:Email.
    let e: Email = json_ida_y_vuelta(r#"{"maxMaskedAddresses":12}"#);
    assert_eq!(e.max_masked_addresses, Some(12));
    pickle_ida_y_vuelta(&e);
}

#[test]
fn el_parche_jmap_acepta_los_campos_de_pago() {
    use jmap_tools::{JsonPointer, JsonPointerItem, Key};
    let mut d = Domain::default();
    let ptr = JsonPointer::new(vec![JsonPointerItem::Key(Key::Property(Property::MemberTenantId))]);
    let r = d.patch(
        JsonPointerPatch::new(&ptr).with_create(true),
        JmapValue::Str(Id::new(5).to_string().into()),
    );
    assert!(r.is_ok(), "el parche de memberTenantId falló");
    assert_eq!(d.member_tenant_id, Some(Id::new(5)));
}
