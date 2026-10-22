//! Checks a regex cannot express: the Luhn checksum behind card numbers, IBAN's mod-97, and whether a JWT's
//! first segment decodes to a JSON header. They cut the false positives of broad patterns.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Validator {
    Luhn,
    IbanMod97,
    JwtHeader,
}

impl Validator {
    pub fn accepts(self, value: &str) -> bool {
        match self {
            Validator::Luhn => luhn(value),
            Validator::IbanMod97 => iban_mod97(value),
            Validator::JwtHeader => jwt_header(value),
        }
    }
}

/// Digits only (separators ignored), 13 to 19 of them, Luhn-valid.
pub fn luhn(value: &str) -> bool {
    let digits: Vec<u32> = value.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len())
        || value
            .chars()
            .any(|c| !(c.is_ascii_digit() || c == ' ' || c == '-'))
    {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                let doubled = d * 2;
                if doubled > 9 { doubled - 9 } else { doubled }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// Two letters, two check digits, 11 to 30 alphanumerics; the rearranged number is 1 modulo 97.
pub fn iban_mod97(value: &str) -> bool {
    let compact: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    if !(15..=34).contains(&compact.len()) || !compact.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let (head, tail) = compact.split_at(4);
    if !head[..2].chars().all(|c| c.is_ascii_uppercase())
        || !head[2..].chars().all(|c| c.is_ascii_digit())
    {
        return false;
    }
    let rearranged = format!("{tail}{head}");
    let mut remainder: u32 = 0;
    for c in rearranged.chars() {
        let piece = match c.to_digit(36) {
            Some(d) => d,
            None => return false,
        };
        let digits = if piece >= 10 { 2 } else { 1 };
        remainder = (remainder * 10u32.pow(digits) + piece) % 97;
    }
    remainder == 1
}

/// The first dot-separated segment is base64url JSON with an `alg` key.
pub fn jwt_header(value: &str) -> bool {
    let Some(head) = value.split('.').next() else {
        return false;
    };
    let Some(bytes) = base64url_decode(head) else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v.get("alg").cloned())
        .is_some()
}

/// Unpadded or padded base64url (and plain base64) to bytes.
pub fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luhn_accepts_test_cards_and_rejects_random_digits() {
        assert!(luhn("4111 1111 1111 1111"));
        assert!(luhn("5500-0000-0000-0004"));
        assert!(luhn("378282246310005"));
        assert!(!luhn("4111 1111 1111 1112"));
        assert!(!luhn("1234567890123"));
        assert!(!luhn("12345"));
        assert!(!luhn("4111a111111111111"));
    }

    #[test]
    fn iban_checks_mod97() {
        assert!(iban_mod97("GB82 WEST 1234 5698 7654 32"));
        assert!(iban_mod97("DE89370400440532013000"));
        assert!(!iban_mod97("GB82 WEST 1234 5698 7654 33"));
        assert!(!iban_mod97("1234567890123456"));
        assert!(!iban_mod97("gb82west12345698765432"));
    }

    #[test]
    fn jwt_headers_must_decode_to_json_with_alg() {
        assert!(jwt_header(
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"
        ));
        assert!(!jwt_header("eyJub3QiOiJqd3QifQ.x.y"), "no alg");
        assert!(!jwt_header("not.a.jwt"));
        assert_eq!(base64url_decode("aGVsbG8"), Some(b"hello".to_vec()));
        assert_eq!(base64url_decode("aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(base64url_decode("a*b"), None);
    }
}
