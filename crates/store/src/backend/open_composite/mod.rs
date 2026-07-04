/*
 * SPDX-FileCopyrightText: 2026 Stalwart contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 *
 * Free-edition composite backends. Read-replica routing implemented purely on
 * top of the public `Store` API, so it works with any SQL/KV backend without
 * the proprietary SEL composite stores.
 */

pub mod read_replica;
