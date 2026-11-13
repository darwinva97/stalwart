/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

#![warn(clippy::large_futures)]

#[cfg(feature = "bezenti")]
pub mod bezenti;
pub mod cache;
pub mod identity;
pub mod mailbox;
pub mod message;
pub mod presence;
pub mod push;
pub mod sieve;
pub mod submission;
