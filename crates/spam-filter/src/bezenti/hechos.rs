/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Registro de hechos que se envía al modelo (el modelo nunca ve el mensaje en crudo).
//!
//! Contenido según la documentación pública («Facts sent to the model»); los nombres de los campos son
//! decisión bezenti (ver `bezenti/especificaciones/ia.md`, §5.3):
//!
//! ```json
//! {"email": {
//!   "from": {"name": "…", "address": "…", "domain": "…"},
//!   "envelopeFrom": "…",
//!   "replyTo": {"name": "…", "address": "…", "domain": "…"} | null,
//!   "recipients": {"count": 2, "domains": ["…"]},
//!   "authentication": {"spf": "pass", "dkim": "pass", "dmarc": "pass", "arc": "none",
//!                      "dkimPassDomains": ["…"]},            // solo con includeAuth
//!   "subject": "…", "isReply": false, "listUnsubscribe": true,
//!   "links": [{"text": "…", "host": "…"}],                    // solo con includeLinks, hasta maxLinks
//!   "attachments": [{"name": "…", "contentType": "…"}],      // solo con includeAttachments
//!   "body": "…", "bodyTruncated": false                      // sin citas ni firma con trimQuotes
//! }}
//! ```

use crate::{
    Recipient, SpamFilterContext, TextPart,
    modules::html::{A, HREF, HtmlToken},
};
use common::bezenti::ia::SpamAiConfig;
use mail_auth::{DkimResult, DmarcResult, SpfResult};
use mail_parser::HeaderName;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub fn build(ctx: &SpamFilterContext<'_>, config: &SpamAiConfig) -> Value {
    let input = &ctx.input;
    let output = &ctx.output;

    let mut is_reply = false;
    let mut list_unsubscribe = false;
    for header in input.message.headers() {
        match header.name() {
            HeaderName::InReplyTo | HeaderName::References => is_reply = true,
            HeaderName::ListUnsubscribe => list_unsubscribe = true,
            _ => {}
        }
    }

    let recipients = output
        .recipients_to
        .iter()
        .chain(output.recipients_cc.iter())
        .collect::<Vec<_>>();
    let recipient_domains = recipients
        .iter()
        .map(|r| r.email.domain_part.fqdn.as_str())
        .filter(|d| !d.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(10)
        .collect::<Vec<_>>();

    let mut email = serde_json::Map::new();
    email.insert("from".into(), recipient(&output.from));
    email.insert("envelopeFrom".into(), input.env_from.into());
    email.insert(
        "replyTo".into(),
        output.reply_to.as_ref().map_or(Value::Null, recipient),
    );
    email.insert(
        "recipients".into(),
        json!({"count": recipients.len().max(input.env_rcpt_orig_to.len()), "domains": recipient_domains}),
    );

    if config.include_auth {
        let dkim_pass_domains = input
            .dkim_result
            .iter()
            .filter(|r| matches!(r.result(), DkimResult::Pass))
            .filter_map(|r| r.signature().map(|s| s.d.to_ascii_lowercase()))
            .collect::<BTreeSet<_>>();
        email.insert(
            "authentication".into(),
            json!({
                "spf": input.spf_mail_from_result.map_or("none", |r| spf(r.result())),
                "dkim": input
                    .dkim_result
                    .iter()
                    .find(|r| matches!(r.result(), DkimResult::Pass))
                    .or_else(|| input.dkim_result.first())
                    .map_or("none", |r| dkim(r.result())),
                "dmarc": input.dmarc_result.map_or("none", dmarc),
                "arc": input.arc_result.map_or("none", |r| dkim(r.result())),
                "dkimPassDomains": dkim_pass_domains,
            }),
        );
    }

    email.insert("subject".into(), output.subject.as_str().into());
    email.insert("isReply".into(), is_reply.into());
    email.insert("listUnsubscribe".into(), list_unsubscribe.into());

    if config.include_links {
        email.insert("links".into(), links(ctx, config.max_links).into());
    }

    let attachments = attachments(ctx, config.max_attachments);
    if config.include_attachments {
        email.insert("attachments".into(), attachments.into());
    }

    let body = ctx.text_body().unwrap_or_default();
    let body = if config.trim_quotes {
        trim_quotes(body)
    } else {
        body.to_string()
    };
    let truncated =
        common::bezenti::ia::cliente::truncate_chars(body.trim(), config.max_input_chars);
    email.insert(
        "bodyTruncated".into(),
        (truncated.len() < body.trim().len()).into(),
    );
    email.insert("body".into(), truncated.into());

    json!({ "email": Value::Object(email) })
}

/// ¿Tiene el mensaje texto o adjuntos? (motivo `no-content` de la compuerta).
pub fn has_content(ctx: &SpamFilterContext<'_>) -> bool {
    ctx.text_body().is_some_and(|b| !b.trim().is_empty())
        || ctx.input.message.attachments().len() > 0
}

fn recipient(r: &Recipient) -> Value {
    json!({
        "name": r.name.as_deref().unwrap_or_default(),
        "address": r.email.address,
        "domain": r.email.domain_part.fqdn,
    })
}

fn spf(r: SpfResult) -> &'static str {
    match r {
        SpfResult::Pass => "pass",
        SpfResult::Fail => "fail",
        SpfResult::SoftFail => "softfail",
        SpfResult::Neutral => "neutral",
        SpfResult::TempError => "temperror",
        SpfResult::PermError => "permerror",
        SpfResult::None => "none",
    }
}

fn dkim(r: &DkimResult) -> &'static str {
    match r {
        DkimResult::Pass => "pass",
        DkimResult::Fail(_) => "fail",
        DkimResult::PermError(_) => "permerror",
        DkimResult::TempError(_) => "temperror",
        DkimResult::Neutral(_) => "neutral",
        DkimResult::None => "none",
    }
}

fn dmarc(r: &DmarcResult) -> &'static str {
    match r {
        DmarcResult::Pass => "pass",
        DmarcResult::Fail(_) => "fail",
        DmarcResult::TempError(_) => "temperror",
        DmarcResult::PermError(_) => "permerror",
        DmarcResult::None => "none",
    }
}

/// Enlaces: texto visible y destino real (después de seguir redirectores, si el filtro lo resolvió).
fn links(ctx: &SpamFilterContext<'_>, max: usize) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let host_of = |href: &str| -> Option<String> {
        let href = href.trim();
        ctx.output
            .urls
            .iter()
            .find(|u| u.element.url_original.trim() == href || u.element.url == href)
            .and_then(|u| {
                u.element
                    .final_host
                    .as_deref()
                    .map(str::to_string)
                    .or_else(|| u.element.url_parsed.as_ref().map(|p| p.host.fqdn.clone()))
            })
            .or_else(|| {
                href.parse::<hyper::Uri>()
                    .ok()
                    .and_then(|u| u.host().map(|h| h.to_ascii_lowercase()))
            })
    };

    // Enlaces HTML con su texto.
    'parts: for part in &ctx.output.text_parts {
        let TextPart::Html { html_tokens, .. } = part else {
            continue;
        };
        let mut current: Option<(String, String)> = None;
        for token in html_tokens {
            match token {
                HtmlToken::StartTag {
                    name, attributes, ..
                } if *name == A => {
                    current = attributes
                        .iter()
                        .find(|(k, _)| *k == HREF)
                        .and_then(|(_, v)| v.as_ref())
                        .map(|href| (href.to_string(), String::new()));
                }
                HtmlToken::Text { text } => {
                    if let Some((_, link_text)) = current.as_mut()
                        && link_text.len() < 200
                    {
                        if !link_text.is_empty() {
                            link_text.push(' ');
                        }
                        link_text.push_str(text.trim());
                    }
                }
                HtmlToken::EndTag { name } if *name == A => {
                    if let Some((href, text)) = current.take()
                        && let Some(host) = host_of(&href)
                        && seen.insert((text.clone(), host.clone()))
                    {
                        out.push(json!({
                            "text": common::bezenti::ia::cliente::truncate_chars(text.trim(), 120),
                            "host": host,
                        }));
                        if out.len() >= max {
                            break 'parts;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Enlaces sin texto (en texto plano).
    if out.len() < max {
        for url in &ctx.output.urls {
            let host = url
                .element
                .final_host
                .as_deref()
                .map(str::to_string)
                .or_else(|| url.element.url_parsed.as_ref().map(|p| p.host.fqdn.clone()));
            if let Some(host) = host
                && !out.iter().any(|l| l["host"] == host.as_str())
                && seen.insert((String::new(), host.clone()))
            {
                out.push(json!({"text": "", "host": host}));
                if out.len() >= max {
                    break;
                }
            }
        }
    }
    out
}

fn attachments(ctx: &SpamFilterContext<'_>, max: usize) -> Vec<Value> {
    ctx.input
        .message
        .attachments()
        .take(max)
        .map(|part| {
            let content_type = part
                .content_type()
                .map(|ct| match ct.subtype() {
                    Some(sub) => format!("{}/{}", ct.ctype(), sub),
                    None => ct.ctype().to_string(),
                })
                .unwrap_or_else(|| "application/octet-stream".into())
                .to_ascii_lowercase();
            json!({
                "name": part.attachment_name().unwrap_or_default(),
                "contentType": content_type,
            })
        })
        .collect()
}

/// Quita citas (`>`), la línea «… wrote:» que las presenta y la firma (a partir de `-- `).
pub fn trim_quotes(text: &str) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if *line == "-- " || line.trim_end() == "--" {
            break;
        }
        if trimmed.starts_with('>') {
            continue;
        }
        let lower = trimmed.trim_end().to_lowercase();
        if (lower.ends_with("wrote:")
            || lower.ends_with("escribió:")
            || lower.ends_with("a écrit :"))
            && lines[idx + 1..]
                .iter()
                .find(|l| !l.trim().is_empty())
                .is_some_and(|l| l.trim_start().starts_with('>'))
        {
            continue;
        }
        if trimmed.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn quita_citas_y_firma() {
        let text = "Hola Ana,\n\nTe confirmo la reunión.\n\n\n\nOn Mon, Ana wrote:\n> ¿Nos vemos?\n> Gracias\n-- \nPedro\nTel 123\n";
        assert_eq!(
            trim_quotes(text),
            "Hola Ana,\n\nTe confirmo la reunión.\n\n"
        );
    }
}
