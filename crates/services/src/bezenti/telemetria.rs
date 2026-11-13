/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Tareas periódicas de la telemetría (edición bezenti, fase 5): recoger el historial de métricas y evaluar las
//! alertas. Especificación `bezenti/especificaciones/telemetria.md`, §4 y §6.
//!
//! Las alertas tienen **histéresis**: se avisa una vez cuando la condición pasa a cumplirse y otra cuando deja de
//! cumplirse («resuelta»); mientras sigue igual no se repite nada. El estado vive en memoria (tras reiniciar, una
//! condición que sigue cumpliéndose vuelve a avisar una vez).

use common::{
    BuildServer, Inner, Server,
    bezenti::telemetria::{
        Alerta, AlertaCorreo, ENV_INTERVALO_ALERTAS, ENV_INTERVALO_METRICAS, INTERVALO_ALERTAS,
        metricas::{Recolector, tomar_inquilinos},
        nodo, registrar_servidor, segundos_env,
    },
    expr::{Bump, functions::EmptyResolver},
};
use mail_builder::{MessageBuilder, headers::HeaderType};
use smtp::reporting::send::MtaReportSend;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use store::{ahash::AHashMap, write::now};
use trc::{Collector, MetricType, TelemetryEvent};

pub fn arrancar(inner: Arc<Inner>) {
    registrar_servidor(&inner);

    // Historial de métricas.
    let inner_ = inner.clone();
    tokio::spawn(async move {
        let mut recolector = Recolector::default();
        loop {
            let espera = segundos_env(ENV_INTERVALO_METRICAS).unwrap_or_else(|| {
                inner_
                    .build_server()
                    .core
                    .telemetria
                    .intervalo_metricas
                    .as_ref()
                    .map(|c| c.time_to_next())
                    .unwrap_or(Duration::from_secs(3600))
            });
            tokio::time::sleep(espera).await;
            let server = inner_.build_server();
            let inquilinos = tomar_inquilinos();
            if server.core.telemetria.metricas.is_none() {
                continue;
            }
            let inicio = Instant::now();
            server.inner.refresh_live_gauges();
            let entradas = recolector.muestrear();
            match server
                .guardar_metricas(now(), nodo(&server), &entradas, &inquilinos)
                .await
            {
                Ok(()) => {
                    trc::event!(
                        Telemetry(TelemetryEvent::MetricsStored),
                        Total = entradas.len(),
                        Elapsed = inicio.elapsed()
                    );
                }
                Err(err) => {
                    trc::error!(err.details("No se pudo guardar el historial de métricas"));
                }
            }
        }
    });

    // Alertas.
    tokio::spawn(async move {
        let intervalo = segundos_env(ENV_INTERVALO_ALERTAS).unwrap_or(INTERVALO_ALERTAS);
        let mut disparadas: AHashMap<u64, bool> = AHashMap::new();
        let mut arena = Bump::new();
        loop {
            tokio::time::sleep(intervalo).await;
            let server = inner.build_server();
            let alertas = server.core.telemetria.alertas.clone();
            disparadas.retain(|id, _| alertas.iter().any(|a| a.id == *id));
            if alertas.is_empty() {
                continue;
            }
            server.inner.refresh_live_gauges();
            for alerta in alertas.iter() {
                let cumple = server
                    .eval_if_with(&alerta.condicion, &EmptyResolver, &mut arena, 0, |v| {
                        v.to_bool()
                    })
                    .await
                    .unwrap_or(false);
                let antes = disparadas.get(&alerta.id).copied().unwrap_or(false);
                if cumple != antes {
                    disparadas.insert(alerta.id, cumple);
                    avisar(&server, alerta, cumple).await;
                }
            }
        }
    });
}

/// Valor de una métrica para los mensajes (`%{queue.count}%`): los histogramas, su media.
fn valor(metrica: MetricType) -> String {
    let v = Collector::read_metric(metrica);
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}

/// Sustituye `%{<métrica>}%` por su valor; los nombres desconocidos se dejan tal cual.
pub(crate) fn rellenar(texto: &str) -> String {
    let mut salida = String::with_capacity(texto.len());
    let mut resto = texto;
    while let Some(inicio) = resto.find("%{") {
        salida.push_str(&resto[..inicio]);
        let tras = &resto[inicio + 2..];
        match tras.find("}%") {
            Some(fin) => {
                let nombre = &tras[..fin];
                match MetricType::parse(nombre.trim()) {
                    Some(m) => salida.push_str(&valor(m)),
                    None => {
                        salida.push_str("%{");
                        salida.push_str(nombre);
                        salida.push_str("}%");
                    }
                }
                resto = &tras[fin + 2..];
            }
            None => {
                salida.push_str(&resto[inicio..]);
                resto = "";
            }
        }
    }
    salida.push_str(resto);
    salida
}

async fn avisar(server: &Server, alerta: &Alerta, disparada: bool) {
    let estado = if disparada { "triggered" } else { "resolved" };
    if let Some(mensaje) = &alerta.evento {
        trc::event!(
            Telemetry(TelemetryEvent::AlertEvent),
            Id = alerta.id,
            Type = estado,
            Details = rellenar(mensaje),
        );
    }
    if let Some(correo) = &alerta.correo {
        enviar_correo(server, alerta.id, correo, disparada).await;
    }
}

async fn enviar_correo(server: &Server, id: u64, correo: &AlertaCorreo, disparada: bool) {
    let asunto = rellenar(&correo.asunto);
    let asunto = if disparada {
        asunto
    } else {
        format!("[Resolved] {asunto}")
    };
    let cuerpo = rellenar(&correo.cuerpo);
    let cuerpo = if disparada {
        cuerpo
    } else {
        format!("The alert condition no longer holds.\r\n\r\n{cuerpo}")
    };
    let de = correo.nombre.as_deref().unwrap_or("");
    let mensaje = MessageBuilder::new()
        .from((de, correo.de.as_str()))
        .to(correo.para.iter().map(|a| a.as_str()).collect::<Vec<_>>())
        .header("Auto-Submitted", HeaderType::Text("auto-generated".into()))
        .message_id(server.core.network.message_id())
        .subject(asunto.as_str())
        .text_body(cuerpo)
        .write_to_vec()
        .unwrap_or_default();
    server
        .send_autogenerated(correo.de.as_str(), correo.para.iter(), mensaje, None, 0)
        .await;
    trc::event!(
        Telemetry(TelemetryEvent::AlertMessage),
        Id = id,
        From = correo.de.clone(),
        To = correo
            .para
            .iter()
            .map(|a| trc::Value::String(a.as_str().into()))
            .collect::<Vec<_>>(),
        Details = asunto,
    );
}

#[cfg(test)]
mod tests {
    use super::rellenar;

    #[test]
    fn marcadores() {
        let t = rellenar("cola %{queue.count}% y %{no.existe}% y %{");
        assert!(t.starts_with("cola "));
        assert!(t.contains("%{no.existe}%"));
        assert!(t.ends_with("%{"));
        assert!(!t.contains("%{queue.count}%"));
    }
}
