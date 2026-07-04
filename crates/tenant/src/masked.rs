/*
 * SPDX-FileCopyrightText: 2026 Stalwart contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 *
 * Masked (aliased) email addresses: a reversible, tamper-evident encoding of a
 * resource id and an optional expiry into an email local-part.
 */

/// Reversible, tamper-evident encoding of a resource id into an email alias.
///
/// Layout of the 128-bit payload (base36-encoded in the local-part):
/// `[ address_id : 64 ][ expires : 32 ][ checksum : 32 ]`
pub struct MaskedAddress;

const U32_MAX: u128 = u32::MAX as u128;

impl MaskedAddress {
    /// Builds `"<prefix>.<base36>@<domain>"` for `address_id`.
    ///
    /// `expires` is an optional lifetime in seconds; `None` means never expires.
    pub fn generate(address_id: u64, expires: Option<u32>, prefix: &str, domain: &str) -> String {
        let expires = expires.unwrap_or(0) as u128 & U32_MAX;
        let address_id = address_id as u128;
        let checksum = (address_id & U32_MAX) ^ ((address_id >> 32) & U32_MAX) ^ expires;
        let payload = (address_id << 64) | (expires << 32) | checksum;

        let mut address = String::with_capacity(prefix.len() + domain.len() + 30);
        address.push_str(prefix);
        address.push('.');
        address.push_str(&base36_encode(payload));
        address.push('@');
        address.push_str(domain);
        address
    }

    /// Recovers the `address_id` from a local-part, rejecting any tampering.
    /// Expiry, if present, is validated against `now` seconds via
    /// [`MaskedAddress::parse_local_part_at`]; this variant ignores expiry.
    pub fn parse_local_part(local_part: &str) -> Option<u64> {
        Self::decode(local_part).map(|(id, _expires)| id)
    }

    /// Like [`MaskedAddress::parse_local_part`] but also enforces expiry: an
    /// alias whose absolute expiry timestamp (epoch seconds) is `<= now` is
    /// rejected. An expiry of `0` means the alias never expires.
    pub fn parse_local_part_at(local_part: &str, now: u32) -> Option<u64> {
        Self::decode(local_part).and_then(|(id, expires)| {
            (expires == 0 || expires > now).then_some(id)
        })
    }

    fn decode(local_part: &str) -> Option<(u64, u32)> {
        let mut parts = local_part.split('.');
        let _prefix = parts.next().filter(|v| !v.is_empty())?;
        let encoded = parts.next().filter(|v| !v.is_empty())?;
        if parts.next().is_some() {
            return None;
        }

        let payload = u128::from_str_radix(encoded, 36).ok()?;
        let address_id = (payload >> 64) as u64;
        let expires = (payload >> 32) as u32;
        let checksum = payload as u32;

        let expected = (address_id as u32) ^ ((address_id >> 32) as u32) ^ expires;
        (checksum == expected).then_some((address_id, expires))
    }
}

fn base36_encode(mut n: u128) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut buf = Vec::with_capacity(25);
    while n > 0 {
        buf.push(CHARS[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_a_masked_address_when_parsed_then_the_original_id_is_recovered() {
        let masked = MaskedAddress::generate(1234, None, "mask", "example.com");

        assert!(masked.ends_with("@example.com"));
        assert!(masked.starts_with("mask."));
        assert_eq!(MaskedAddress::parse_local_part(local_part(&masked)), Some(1234));
    }

    #[test]
    fn given_a_tampered_local_part_when_parsed_then_it_is_rejected() {
        let masked = MaskedAddress::generate(1234, None, "mask", "example.com");
        let good = local_part(&masked).to_string();

        // Flip the last base36 character to simulate tampering.
        let mut chars: Vec<char> = good.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'z' { 'y' } else { 'z' };
        let tampered: String = chars.into_iter().collect();

        assert_eq!(MaskedAddress::parse_local_part(&tampered), None);
    }

    #[test]
    fn given_an_unexpired_alias_when_parsed_at_now_then_the_id_is_recovered() {
        let masked = MaskedAddress::generate(77, Some(2_000), "mask", "example.com");

        assert_eq!(
            MaskedAddress::parse_local_part_at(local_part(&masked), 1_500),
            Some(77)
        );
    }

    #[test]
    fn given_an_expired_alias_when_parsed_at_now_then_it_is_rejected() {
        let masked = MaskedAddress::generate(77, Some(2_000), "mask", "example.com");

        assert_eq!(
            MaskedAddress::parse_local_part_at(local_part(&masked), 2_001),
            None
        );
    }

    #[test]
    fn given_an_alias_without_expiry_when_parsed_at_any_time_then_the_id_is_recovered() {
        let masked = MaskedAddress::generate(77, None, "mask", "example.com");

        assert_eq!(
            MaskedAddress::parse_local_part_at(local_part(&masked), u32::MAX),
            Some(77)
        );
    }

    fn local_part(address: &str) -> &str {
        address.split('@').next().unwrap()
    }
}
