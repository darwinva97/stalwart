/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

// Archivo generado por bezenti/generador/gen_inquilino.py a partir del esquema. No se edita a mano.

use crate::schema::prelude::*;

impl ObjectInner {
    /// Inquilino al que pertenece el objeto (`memberTenantId`), o `None` si no tiene o no es de un inquilino.
    pub fn member_tenant_id(&self) -> Option<Id> {
        match self {
            ObjectInner::Account(inner) => match inner.as_ref() {
                Account::User(obj) => obj.member_tenant_id,
                Account::Group(obj) => obj.member_tenant_id,
            },
            ObjectInner::AcmeProvider(obj) => obj.member_tenant_id,
            ObjectInner::ArfExternalReport(obj) => obj.member_tenant_id,
            ObjectInner::Directory(inner) => match inner.as_ref() {
                Directory::Ldap(obj) => obj.member_tenant_id,
                Directory::Sql(obj) => obj.member_tenant_id,
                Directory::Oidc(obj) => obj.member_tenant_id,
            },
            ObjectInner::DkimSignature(inner) => match inner.as_ref() {
                DkimSignature::Dkim1Ed25519Sha256(obj) => obj.member_tenant_id,
                DkimSignature::Dkim1RsaSha256(obj) => obj.member_tenant_id,
                DkimSignature::Dkim2Ed25519Sha256(obj) => obj.member_tenant_id,
                DkimSignature::Dkim2RsaSha256(obj) => obj.member_tenant_id,
                DkimSignature::Dkim2MlDsa44(obj) => obj.member_tenant_id,
            },
            ObjectInner::DmarcExternalReport(obj) => obj.member_tenant_id,
            ObjectInner::DnsServer(inner) => match inner.as_ref() {
                DnsServer::Tsig(obj) => obj.member_tenant_id,
                DnsServer::Cloudflare(obj) => obj.member_tenant_id,
                DnsServer::DigitalOcean(obj) => obj.member_tenant_id,
                DnsServer::DeSEC(obj) => obj.member_tenant_id,
                DnsServer::Ovh(obj) => obj.member_tenant_id,
                DnsServer::Bunny(obj) => obj.member_tenant_id,
                DnsServer::Porkbun(obj) => obj.member_tenant_id,
                DnsServer::Dnsimple(obj) => obj.member_tenant_id,
                DnsServer::Spaceship(obj) => obj.member_tenant_id,
                DnsServer::Route53(obj) => obj.member_tenant_id,
                DnsServer::GoogleCloudDns(obj) => obj.member_tenant_id,
                DnsServer::Alidns(obj) => obj.member_tenant_id,
                DnsServer::ArvanCloud(obj) => obj.member_tenant_id,
                DnsServer::Autodns(obj) => obj.member_tenant_id,
                DnsServer::AzureDns(obj) => obj.member_tenant_id,
                DnsServer::BaiduCloud(obj) => obj.member_tenant_id,
                DnsServer::BluecatV2(obj) => obj.member_tenant_id,
                DnsServer::ClouDns(obj) => obj.member_tenant_id,
                DnsServer::Constellix(obj) => obj.member_tenant_id,
                DnsServer::Cpanel(obj) => obj.member_tenant_id,
                DnsServer::Ddnss(obj) => obj.member_tenant_id,
                DnsServer::DnsMadeEasy(obj) => obj.member_tenant_id,
                DnsServer::Domeneshop(obj) => obj.member_tenant_id,
                DnsServer::Dreamhost(obj) => obj.member_tenant_id,
                DnsServer::DuckDns(obj) => obj.member_tenant_id,
                DnsServer::Dynu(obj) => obj.member_tenant_id,
                DnsServer::EasyDns(obj) => obj.member_tenant_id,
                DnsServer::EdgeDns(obj) => obj.member_tenant_id,
                DnsServer::Exoscale(obj) => obj.member_tenant_id,
                DnsServer::FreeMyIp(obj) => obj.member_tenant_id,
                DnsServer::GandiV5(obj) => obj.member_tenant_id,
                DnsServer::Gcore(obj) => obj.member_tenant_id,
                DnsServer::Glesys(obj) => obj.member_tenant_id,
                DnsServer::Godaddy(obj) => obj.member_tenant_id,
                DnsServer::Hetzner(obj) => obj.member_tenant_id,
                DnsServer::HostingDe(obj) => obj.member_tenant_id,
                DnsServer::Hostinger(obj) => obj.member_tenant_id,
                DnsServer::HuaweiCloud(obj) => obj.member_tenant_id,
                DnsServer::Hurricane(obj) => obj.member_tenant_id,
                DnsServer::IbmCloud(obj) => obj.member_tenant_id,
                DnsServer::Infoblox(obj) => obj.member_tenant_id,
                DnsServer::Infomaniak(obj) => obj.member_tenant_id,
                DnsServer::Inwx(obj) => obj.member_tenant_id,
                DnsServer::Ionos(obj) => obj.member_tenant_id,
                DnsServer::Ipv64(obj) => obj.member_tenant_id,
                DnsServer::Joker(obj) => obj.member_tenant_id,
                DnsServer::Lightsail(obj) => obj.member_tenant_id,
                DnsServer::Linode(obj) => obj.member_tenant_id,
                DnsServer::LuaDns(obj) => obj.member_tenant_id,
                DnsServer::MythicBeasts(obj) => obj.member_tenant_id,
                DnsServer::Namecheap(obj) => obj.member_tenant_id,
                DnsServer::NameDotCom(obj) => obj.member_tenant_id,
                DnsServer::NameSilo(obj) => obj.member_tenant_id,
                DnsServer::Netcup(obj) => obj.member_tenant_id,
                DnsServer::Netlify(obj) => obj.member_tenant_id,
                DnsServer::Nifcloud(obj) => obj.member_tenant_id,
                DnsServer::Ns1(obj) => obj.member_tenant_id,
                DnsServer::OracleCloud(obj) => obj.member_tenant_id,
                DnsServer::Plesk(obj) => obj.member_tenant_id,
                DnsServer::Safedns(obj) => obj.member_tenant_id,
                DnsServer::Scaleway(obj) => obj.member_tenant_id,
                DnsServer::TencentCloud(obj) => obj.member_tenant_id,
                DnsServer::Transip(obj) => obj.member_tenant_id,
                DnsServer::UltraDns(obj) => obj.member_tenant_id,
                DnsServer::Vercel(obj) => obj.member_tenant_id,
                DnsServer::Volcengine(obj) => obj.member_tenant_id,
                DnsServer::Vultr(obj) => obj.member_tenant_id,
                DnsServer::WebSupport(obj) => obj.member_tenant_id,
                DnsServer::YandexCloud(obj) => obj.member_tenant_id,
                DnsServer::PowerDns(obj) => obj.member_tenant_id,
            },
            ObjectInner::Domain(obj) => obj.member_tenant_id,
            ObjectInner::MailingList(obj) => obj.member_tenant_id,
            ObjectInner::OAuthClient(obj) => obj.member_tenant_id,
            ObjectInner::Role(obj) => obj.member_tenant_id,
            ObjectInner::TlsExternalReport(obj) => obj.member_tenant_id,
            _ => None,
        }
    }

    /// Cambia el inquilino del objeto. Devuelve `false` si este tipo de objeto no tiene `memberTenantId`.
    pub fn set_member_tenant_id(&mut self, tenant_id: Option<Id>) -> bool {
        match self {
            ObjectInner::Account(inner) => match inner.as_mut() {
                Account::User(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                Account::Group(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
            },
            ObjectInner::AcmeProvider(obj) => {
                obj.member_tenant_id = tenant_id;
                true
            }
            ObjectInner::ArfExternalReport(obj) => {
                obj.member_tenant_id = tenant_id;
                true
            }
            ObjectInner::Directory(inner) => match inner.as_mut() {
                Directory::Ldap(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                Directory::Sql(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                Directory::Oidc(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
            },
            ObjectInner::DkimSignature(inner) => match inner.as_mut() {
                DkimSignature::Dkim1Ed25519Sha256(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DkimSignature::Dkim1RsaSha256(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DkimSignature::Dkim2Ed25519Sha256(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DkimSignature::Dkim2RsaSha256(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DkimSignature::Dkim2MlDsa44(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
            },
            ObjectInner::DmarcExternalReport(obj) => {
                obj.member_tenant_id = tenant_id;
                true
            }
            ObjectInner::DnsServer(inner) => match inner.as_mut() {
                DnsServer::Tsig(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Cloudflare(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::DigitalOcean(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::DeSEC(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Ovh(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Bunny(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Porkbun(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Dnsimple(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Spaceship(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Route53(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::GoogleCloudDns(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Alidns(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::ArvanCloud(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Autodns(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::AzureDns(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::BaiduCloud(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::BluecatV2(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::ClouDns(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Constellix(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Cpanel(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Ddnss(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::DnsMadeEasy(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Domeneshop(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Dreamhost(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::DuckDns(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Dynu(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::EasyDns(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::EdgeDns(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Exoscale(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::FreeMyIp(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::GandiV5(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Gcore(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Glesys(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Godaddy(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Hetzner(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::HostingDe(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Hostinger(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::HuaweiCloud(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Hurricane(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::IbmCloud(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Infoblox(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Infomaniak(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Inwx(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Ionos(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Ipv64(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Joker(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Lightsail(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Linode(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::LuaDns(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::MythicBeasts(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Namecheap(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::NameDotCom(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::NameSilo(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Netcup(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Netlify(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Nifcloud(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Ns1(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::OracleCloud(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Plesk(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Safedns(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Scaleway(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::TencentCloud(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Transip(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::UltraDns(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Vercel(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Volcengine(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::Vultr(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::WebSupport(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::YandexCloud(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
                DnsServer::PowerDns(obj) => {
                    obj.member_tenant_id = tenant_id;
                    true
                }
            },
            ObjectInner::Domain(obj) => {
                obj.member_tenant_id = tenant_id;
                true
            }
            ObjectInner::MailingList(obj) => {
                obj.member_tenant_id = tenant_id;
                true
            }
            ObjectInner::OAuthClient(obj) => {
                obj.member_tenant_id = tenant_id;
                true
            }
            ObjectInner::Role(obj) => {
                obj.member_tenant_id = tenant_id;
                true
            }
            ObjectInner::TlsExternalReport(obj) => {
                obj.member_tenant_id = tenant_id;
                true
            }
            _ => false,
        }
    }
}
