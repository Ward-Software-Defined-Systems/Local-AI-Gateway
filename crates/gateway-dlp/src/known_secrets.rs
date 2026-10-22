//! Exact secret values the operator asks the gateway to watch for, matched in every form they travel in:
//! raw, percent-encoded, base64 (standard and URL-safe, padded or not), and the bare token of a
//! `Bearer <token>` header. Values are kept in memory only and reported by their kind, never their value.
//! Ported from an earlier in-house `SecretSet`, extended to return every match span so stored copies can be redacted.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Values shorter than this are not worth matching: they would hit everywhere.
pub const MIN_LEN: usize = 8;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownSecretsFile {
    #[serde(default)]
    pub secret: Vec<KnownSecretEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownSecretEntry {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownMatch {
    pub kind: String,
    pub start: usize,
    pub end: usize,
    /// The form the value was found in.
    pub form: &'static str,
}

#[derive(Debug, Default, Clone)]
pub struct KnownSecrets {
    /// Needle → (kind, form). Several needles per secret, one per encoding.
    needles: BTreeMap<String, (String, &'static str)>,
    count: usize,
}

impl KnownSecrets {
    pub fn parse(toml_text: &str) -> anyhow::Result<Self> {
        let file: KnownSecretsFile = toml::from_str(toml_text)?;
        Ok(Self::from_entries(
            file.secret.into_iter().map(|e| (e.kind, e.value)),
        ))
    }

    pub fn from_entries(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut set = Self::default();
        for (kind, value) in entries {
            set.add(&kind, &value);
        }
        set
    }

    pub fn add(&mut self, kind: &str, value: &str) {
        let value = value.trim();
        if value.len() < MIN_LEN {
            return;
        }
        self.count += 1;
        // The value as given, and the bare token when it was given as a `Bearer <token>` header value;
        // each travels raw, percent-encoded or base64-encoded.
        let mut bases: Vec<(&str, &'static str)> = vec![(value, "raw")];
        if let Some(token) = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
        {
            let token = token.trim();
            if token.len() >= MIN_LEN {
                bases.push((token, "bearer"));
            }
        }
        for (base, raw_form) in bases {
            self.insert(kind, base.to_string(), raw_form);
            self.insert(kind, percent_encode(base), "percent");
            self.insert(kind, base64_encode(base.as_bytes(), false, true), "base64");
            self.insert(kind, base64_encode(base.as_bytes(), false, false), "base64");
            self.insert(
                kind,
                base64_encode(base.as_bytes(), true, true),
                "base64url",
            );
            self.insert(
                kind,
                base64_encode(base.as_bytes(), true, false),
                "base64url",
            );
        }
    }

    fn insert(&mut self, kind: &str, needle: String, form: &'static str) {
        if needle.len() >= MIN_LEN {
            self.needles
                .entry(needle)
                .or_insert((kind.to_string(), form));
        }
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Every occurrence of every form, as byte spans into `text`. Overlapping hits (an unpadded base64
    /// needle is a prefix of the padded one; a bare token sits inside its `Bearer` form) collapse to the
    /// earliest-starting, longest span.
    pub fn find_all(&self, text: &str) -> Vec<KnownMatch> {
        let mut out = Vec::new();
        for (needle, (kind, form)) in &self.needles {
            let mut from = 0;
            while let Some(pos) = text[from..].find(needle.as_str()) {
                let start = from + pos;
                out.push(KnownMatch {
                    kind: kind.clone(),
                    start,
                    end: start + needle.len(),
                    form,
                });
                from = start + needle.len();
            }
        }
        out.sort_by_key(|m| (m.start, std::cmp::Reverse(m.end)));
        let mut kept: Vec<KnownMatch> = Vec::with_capacity(out.len());
        for m in out {
            if kept.last().is_some_and(|last| m.start < last.end) {
                continue;
            }
            kept.push(m);
        }
        kept
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn base64_encode(bytes: &[u8], url_safe: bool, padded: bool) -> String {
    const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let table = if url_safe { URL } else { STD };
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(table[(n >> 18) as usize & 63] as char);
        out.push(table[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(table[(n >> 6) as usize & 63] as char);
        } else if padded {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(table[n as usize & 63] as char);
        } else if padded {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_is_found_in_every_form_and_short_values_are_ignored() {
        let set = KnownSecrets::from_entries([
            ("github".to_string(), "ghp_abcdefghijklmnop".to_string()),
            ("short".to_string(), "abc".to_string()),
        ]);
        assert_eq!(set.len(), 1, "the short value is dropped");
        let raw = set.find_all("token=ghp_abcdefghijklmnop end");
        assert_eq!(raw.len(), 1);
        assert_eq!(
            (raw[0].kind.as_str(), raw[0].form, raw[0].start, raw[0].end),
            ("github", "raw", 6, 26)
        );
        let b64 = base64_encode(b"ghp_abcdefghijklmnop", false, true);
        let hit = set.find_all(&format!("Authorization: Basic {b64}"));
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].form, "base64");
        let url = base64_encode(b"ghp_abcdefghijklmnop", true, false);
        let form = set.find_all(&url)[0].form;
        assert!(
            form.starts_with("base64"),
            "{form}: the URL-safe form equals the standard one for this value"
        );
        // 0xC3 0xBF bytes encode to `w7/D…`, so the URL-safe form differs from the standard one.
        let set = KnownSecrets::from_entries([("x".to_string(), "ÿÿÿÿÿÿÿÿ".to_string())]);
        let url = base64_encode("ÿÿÿÿÿÿÿÿ".as_bytes(), true, false);
        assert!(url.contains('_'), "{url}");
        assert_eq!(set.find_all(&url)[0].form, "base64url");
        assert!(set.find_all("nothing here").is_empty());
    }

    #[test]
    fn percent_encoded_and_bearer_forms_match() {
        let set =
            KnownSecrets::from_entries([("api".to_string(), "Bearer sk-live/abc+def".to_string())]);
        let hits = set.find_all("Authorization: Bearer sk-live/abc+def and sk-live/abc+def again and sk-live%2Fabc%2Bdef");
        let forms: Vec<&str> = hits.iter().map(|h| h.form).collect();
        assert!(forms.contains(&"raw"), "{forms:?}");
        assert!(forms.contains(&"bearer"), "{forms:?}");
        assert!(forms.contains(&"percent"), "{forms:?}");
        assert!(
            hits.windows(2).all(|w| w[0].start <= w[1].start),
            "sorted by position"
        );
    }

    #[test]
    fn the_file_shape_parses_and_rejects_unknown_keys() {
        let set = KnownSecrets::parse("[[secret]]\nkind = \"vault\"\nvalue = \"hunter2hunter2\"\n")
            .unwrap();
        assert_eq!(set.len(), 1);
        assert!(
            KnownSecrets::parse("[[secret]]\nkind = \"x\"\nvalue = \"y\"\nextra = 1\n").is_err()
        );
        assert!(KnownSecrets::parse("").unwrap().is_empty());
    }

    #[test]
    fn base64_matches_the_standard_alphabets() {
        assert_eq!(base64_encode(b"hello", false, true), "aGVsbG8=");
        assert_eq!(base64_encode(b"hello", false, false), "aGVsbG8");
        assert_eq!(base64_encode(b"\xfb\xff", true, true), "-_8=");
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
    }
}
