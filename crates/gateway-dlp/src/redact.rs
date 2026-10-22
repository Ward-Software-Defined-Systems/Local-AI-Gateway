//! Redaction of stored copies. A finding's value is replaced by `[REDACTED:<rule>:<fp8>]`, which holds no
//! quotes or backslashes, so a redacted JSON body stays valid JSON. Spans are applied back to front so
//! earlier offsets stay true.

/// The placeholder that replaces a secret in a stored copy.
pub fn placeholder(rule_id: &str, fingerprint: &str) -> String {
    let short: String = fingerprint.chars().take(8).collect();
    format!("[REDACTED:{rule_id}:{short}]")
}

/// Replaces `(start, end)` byte spans of `text` with their placeholders. Overlapping spans are merged
/// into the earliest one's placeholder.
pub fn redact_spans(text: &str, spans: &[(usize, usize, String)]) -> String {
    let mut sorted: Vec<&(usize, usize, String)> = spans
        .iter()
        .filter(|(s, e, _)| s < e && *e <= text.len())
        .collect();
    sorted.sort_by_key(|(s, e, _)| (*s, std::cmp::Reverse(*e)));
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in sorted {
        if *start < cursor {
            // Overlaps the previous span: extend the cut, keep the earlier placeholder.
            if *end > cursor {
                cursor = *end;
            }
            continue;
        }
        if !text.is_char_boundary(*start) || !text.is_char_boundary(*end) {
            continue;
        }
        out.push_str(&text[cursor..*start]);
        out.push_str(replacement);
        cursor = *end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Replaces every occurrence of each value. Returns the text and whether every value was found at least
/// once, which a caller uses to decide whether a raw stream copy can be stored safely.
pub fn redact_values(text: &str, values: &[(String, String)]) -> (String, bool) {
    let mut out = text.to_string();
    let mut all_found = true;
    let mut ordered: Vec<&(String, String)> =
        values.iter().filter(|(v, _)| !v.is_empty()).collect();
    // Longest values first, so a value that contains another is replaced whole.
    ordered.sort_by_key(|(v, _)| std::cmp::Reverse(v.len()));
    for (value, replacement) in ordered {
        if out.contains(value.as_str()) {
            out = out.replace(value.as_str(), replacement);
        } else if !replacement.is_empty() && !out.contains(replacement.as_str()) {
            all_found = false;
        }
    }
    (out, all_found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_are_replaced_back_to_front_and_overlaps_merge() {
        let text = "key AKIAIOSFODNN7EXAMPLE and token ghp_x end";
        let out = redact_spans(text, &[(35, 40, "[B]".into()), (4, 24, "[A]".into())]);
        assert_eq!(out, "key [A] and token [B] end");
        let out = redact_spans("abcdefgh", &[(1, 5, "[1]".into()), (3, 7, "[2]".into())]);
        assert_eq!(out, "a[1]h");
        assert_eq!(
            redact_spans("short", &[(2, 99, "[x]".into())]),
            "short",
            "an out-of-range span is ignored"
        );
    }

    #[test]
    fn values_are_replaced_longest_first_and_misses_are_reported() {
        let (out, all) = redact_values(
            "sk-abc sk-abcdef",
            &[
                ("sk-abc".into(), "[a]".into()),
                ("sk-abcdef".into(), "[b]".into()),
            ],
        );
        assert_eq!(out, "[a] [b]");
        assert!(all);
        let (out, all) = redact_values("nothing", &[("absent".into(), "[x]".into())]);
        assert_eq!(out, "nothing");
        assert!(!all);
    }

    #[test]
    fn placeholders_are_json_safe() {
        let p = placeholder("aws-access-key-id", "0123456789abcdef0123456789abcdef");
        assert_eq!(p, "[REDACTED:aws-access-key-id:01234567]");
        assert!(!p.contains('"') && !p.contains('\\'));
    }
}
