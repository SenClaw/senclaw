//! Comparing runtime version strings (`docs/runtime-protocol.md` §5.1:
//! "Versions compare numerically (`b11201` > `b9999`; semver for `0.x.y`),
//! never as plain strings"). A string compare puts `b9999` above `b11201`
//! and `0.9.0` above `0.10.0` — exactly backwards for "which is newest".

use std::cmp::Ordering;

/// Compare two version strings the way the Runtime screen needs to.
/// `b<N>` llama.cpp build tags compare by their number; dotted numeric
/// versions (`0.10.0`, `1.2.3-rc1`) compare segment by segment with a
/// `-`-suffixed pre-release sorting below the bare release; anything else
/// falls back to a plain string compare (still a total order, just not a
/// numeric one — good enough for an id this schema does not otherwise use).
pub fn cmp_versions(a: &str, b: &str) -> Ordering {
    if let (Some(na), Some(nb)) = (parse_build_tag(a), parse_build_tag(b)) {
        return na.cmp(&nb);
    }
    if let (Some(pa), Some(pb)) = (parse_dotted(a), parse_dotted(b)) {
        return pa.cmp(&pb);
    }
    a.cmp(b)
}

/// `b11201` → `11201`. Only a `b` followed by digits and nothing else counts
/// — `beta`, bare `b`, and anything with a suffix fall through to the dotted
/// or string comparisons instead.
fn parse_build_tag(v: &str) -> Option<u64> {
    v.strip_prefix('b').filter(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))?.parse().ok()
}

/// `1.2.3-rc1` → `([1,2,3], is_release=false, "rc1")`. `is_release` sorts a
/// bare release above any suffixed pre-release of the same numeric core —
/// `false < true` in Rust's derived `Ord`, so putting the flag before the
/// suffix string in the tuple is what gives `1.2.3 > 1.2.3-rc1`.
fn parse_dotted(v: &str) -> Option<(Vec<u64>, bool, String)> {
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p.to_string())),
        None => (v, None),
    };
    let segments: Vec<u64> = core.split('.').map(|s| s.parse::<u64>().ok()).collect::<Option<_>>()?;
    if segments.is_empty() {
        return None;
    }
    Some((segments, pre.is_none(), pre.unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_tags_compare_numerically_not_lexically() {
        assert_eq!(cmp_versions("b11201", "b9999"), Ordering::Greater, "string compare would say the opposite");
        assert_eq!(cmp_versions("b100", "b100"), Ordering::Equal);
        assert_eq!(cmp_versions("b99", "b100"), Ordering::Less);
    }

    #[test]
    fn dotted_versions_compare_by_segment() {
        assert_eq!(cmp_versions("0.10.0", "0.9.0"), Ordering::Greater, "string compare would say the opposite");
        assert_eq!(cmp_versions("1.2.0", "1.2.0"), Ordering::Equal);
        assert_eq!(cmp_versions("1.2.10", "1.2.9"), Ordering::Greater);
    }

    #[test]
    fn a_pre_release_sorts_below_the_bare_release() {
        assert_eq!(cmp_versions("1.0.0", "1.0.0-rc1"), Ordering::Greater);
        assert_eq!(cmp_versions("1.0.0-rc1", "1.0.0"), Ordering::Less);
    }

    #[test]
    fn unrecognised_shapes_fall_back_to_a_total_string_order_without_panicking() {
        assert_eq!(cmp_versions("latest", "latest"), Ordering::Equal);
        assert_ne!(cmp_versions("b11201", "0.10.0"), Ordering::Equal);
    }
}
