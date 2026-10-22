//! Shannon entropy in bits per character, the usual guard against generic assignment rules matching
//! `password = "password"`.

pub fn shannon_bits_per_char(s: &str) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    let mut total = 0u32;
    for b in s.bytes() {
        counts[b as usize] += 1;
        total += 1;
    }
    let total = total as f32;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f32 / total;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_characters_have_low_entropy_and_random_strings_high() {
        assert_eq!(shannon_bits_per_char(""), 0.0);
        assert_eq!(shannon_bits_per_char("aaaaaaaa"), 0.0);
        assert!(shannon_bits_per_char("password") < 3.0);
        assert!(shannon_bits_per_char("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY") > 4.0);
        assert!(shannon_bits_per_char("AKIAIOSFODNN7EXAMPLE") > 3.5);
    }
}
