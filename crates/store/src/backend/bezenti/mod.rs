/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Almacenes compuestos de la edición bezenti (fase 6): réplicas de lectura SQL y almacenes repartidos.
//!
//! Especificación: `bezenti/especificaciones/replicas-y-reparto.md`. Todo este módulo es código propio, escrito
//! desde la documentación pública y el esquema; no deriva de ningún código de la edición Enterprise.

#[cfg(all(test, any(feature = "postgres", feature = "mysql")))]
mod pruebas_reales;
pub mod reparto;
#[cfg(any(feature = "postgres", feature = "mysql"))]
pub mod replicas;

use xxhash_rust::xxh3::xxh3_64_with_seed;

/// Semilla fija del reparto. Cambiarla cambiaría el fragmento de TODAS las claves: no se toca nunca.
const SEMILLA_REPARTO: u64 = 0x6265_7a65_6e74_6936; // "bezenti6"

/// Fragmento de una clave entre `n` miembros, por *rendezvous hashing* (HRW, «el peso más alto gana»).
///
/// Para cada miembro `i` se calcula `xxh3_64(clave, semilla = SEMILLA_REPARTO + i)` y gana el de la puntuación
/// más alta (en empate, el de índice menor). Es estable entre nodos, versiones y reinicios (XXH3 es un formato
/// congelado) y, a diferencia del módulo `hash % n`, al AÑADIR un miembro al final solo cambian de sitio
/// ~1/(n+1) de las claves: las que pasan a ganar en el miembro nuevo. Quitar o reordenar miembros sí cambia
/// la asignación (el índice forma parte de la semilla).
pub fn fragmento(clave: &[u8], n: usize) -> usize {
    let mut mejor = 0;
    let mut puntuacion_mejor = 0u64;
    for i in 0..n {
        let puntuacion = xxh3_64_with_seed(clave, SEMILLA_REPARTO.wrapping_add(i as u64));
        if i == 0 || puntuacion > puntuacion_mejor {
            mejor = i;
            puntuacion_mejor = puntuacion;
        }
    }
    mejor
}

#[cfg(test)]
mod pruebas {
    use super::fragmento;

    #[test]
    fn reparto_estable_y_equilibrado() {
        // Valores fijos: si alguien cambia el algoritmo o la semilla, esta prueba falla (y con razón: los
        // blobs ya escritos dejarían de encontrarse en su fragmento).
        assert_eq!(fragmento(b"", 3), fragmento(b"", 3));
        let fijos: Vec<usize> = (0u32..8).map(|i| fragmento(&i.to_be_bytes(), 3)).collect();
        assert_eq!(
            fijos, FIJOS_3,
            "el reparto cambió: los datos ya escritos quedarían huérfanos"
        );

        for n in [2usize, 3, 5] {
            let mut cuenta = vec![0usize; n];
            let total = 30_000;
            for i in 0u32..total {
                cuenta[fragmento(blake3::hash(&i.to_le_bytes()).as_bytes(), n)] += 1;
            }
            let ideal = total as f64 / n as f64;
            for c in &cuenta {
                let desvio = (*c as f64 - ideal).abs() / ideal;
                assert!(
                    desvio < 0.05,
                    "reparto desequilibrado con {n} miembros: {cuenta:?}"
                );
            }
        }
    }

    #[test]
    fn anadir_un_miembro_mueve_pocas_claves() {
        let total = 20_000u32;
        let mut movidas = 0;
        for i in 0..total {
            let clave = blake3::hash(&i.to_le_bytes());
            let antes = fragmento(clave.as_bytes(), 3);
            let despues = fragmento(clave.as_bytes(), 4);
            if antes != despues {
                // Solo pueden irse al miembro nuevo, nunca saltar entre los viejos.
                assert_eq!(despues, 3);
                movidas += 1;
            }
        }
        let fraccion = movidas as f64 / total as f64;
        assert!(
            (0.20..0.30).contains(&fraccion),
            "se movió {fraccion} (esperado ~1/4)"
        );
    }

    const FIJOS_3: [usize; 8] = [0, 2, 2, 0, 0, 0, 1, 0];
}
