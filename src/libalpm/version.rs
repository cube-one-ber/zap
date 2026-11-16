//! Version comparison, as `alpm_pkg_vercmp` (pacman's rpmvercmp variant).
use std::cmp::Ordering;

/// Split `[epoch:]version[-release]`. A missing epoch is "0"; a missing
/// release is None.
fn parse_evr(evr: &[u8]) -> (&[u8], &[u8], Option<&[u8]>) {
    let s = evr.iter().position(|b| !b.is_ascii_digit()).unwrap_or(evr.len());
    let se = evr[s..].iter().rposition(|&b| b == b'-').map(|i| s + i);
    let version_end = se.unwrap_or(evr.len());
    let (epoch, version) = if evr.get(s) == Some(&b':') {
        let epoch = if s == 0 { &b"0"[..] } else { &evr[..s] };
        (epoch, &evr[s + 1..version_end.max(s + 1)])
    } else {
        (&b"0"[..], &evr[..version_end])
    };
    (epoch, version, se.map(|at| &evr[at + 1..]))
}

/// Compare alpha and numeric segments of two version strings.
fn rpmvercmp(a: &[u8], b: &[u8]) -> i32 {
    if a == b {
        return 0;
    }
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let (mut one, mut two) = (0usize, 0usize);
    let (mut ptr1, mut ptr2) = (0usize, 0usize);
    while one < a.len() && two < b.len() {
        while one < a.len() && !a[one].is_ascii_alphanumeric() {
            one += 1;
        }
        while two < b.len() && !b[two].is_ascii_alphanumeric() {
            two += 1;
        }
        // Ran to the end of either: finished with the loop.
        if !(one < a.len() && two < b.len()) {
            break;
        }
        // Separator lengths differ.
        if one - ptr1 != two - ptr2 {
            return if one - ptr1 < two - ptr2 { -1 } else { 1 };
        }
        ptr1 = one;
        ptr2 = two;
        let isnum = a[ptr1].is_ascii_digit();
        if isnum {
            while ptr1 < a.len() && a[ptr1].is_ascii_digit() {
                ptr1 += 1;
            }
            while ptr2 < b.len() && b[ptr2].is_ascii_digit() {
                ptr2 += 1;
            }
        } else {
            while ptr1 < a.len() && a[ptr1].is_ascii_alphabetic() {
                ptr1 += 1;
            }
            while ptr2 < b.len() && b[ptr2].is_ascii_alphabetic() {
                ptr2 += 1;
            }
        }
        if one == ptr1 {
            return -1;
        }
        // Segments of different types: numeric is newer than alpha.
        if two == ptr2 {
            return if isnum { 1 } else { -1 };
        }
        let (mut seg1, mut seg2) = (&a[one..ptr1], &b[two..ptr2]);
        if isnum {
            while seg1.first() == Some(&b'0') {
                seg1 = &seg1[1..];
            }
            while seg2.first() == Some(&b'0') {
                seg2 = &seg2[1..];
            }
            if seg1.len() != seg2.len() {
                return if seg1.len() > seg2.len() { 1 } else { -1 };
            }
        }
        match seg1.cmp(seg2) {
            Ordering::Less => return -1,
            Ordering::Greater => return 1,
            Ordering::Equal => {}
        }
        one = ptr1;
        two = ptr2;
    }
    if one >= a.len() && two >= b.len() {
        return 0;
    }
    // A remaining alpha string never beats a released version.
    if (one >= a.len() && !at(b, two).is_ascii_alphabetic()) || at(a, one).is_ascii_alphabetic() {
        -1
    } else {
        1
    }
}

/// Compare two `[epoch:]version[-release]` strings: -1, 0 or 1.
pub fn vercmp(a: &str, b: &str) -> i32 {
    if a == b {
        return 0;
    }
    let (e1, v1, r1) = parse_evr(a.as_bytes());
    let (e2, v2, r2) = parse_evr(b.as_bytes());
    let mut ret = rpmvercmp(e1, e2);
    if ret == 0 {
        ret = rpmvercmp(v1, v2);
        if ret == 0 {
            if let (Some(r1), Some(r2)) = (r1, r2) {
                ret = rpmvercmp(r1, r2);
            }
        }
    }
    ret
}

#[cfg(test)]
mod tests {
    use super::vercmp;

    // Every case from pacman's test/util/vercmptest.sh.
    #[test]
    fn versions_compare_like_pacman() {
        let cases: &[(&str, &str, i32)] = &[
            ("1.5.0", "1.5.0", 0),
            ("1.5.1", "1.5.0", 1),
            ("1.5.1", "1.5", 1),
            ("1.5.0-1", "1.5.0-1", 0),
            ("1.5.0-1", "1.5.0-2", -1),
            ("1.5.0-1", "1.5.1-1", -1),
            ("1.5.0-2", "1.5.1-1", -1),
            ("1.5-1", "1.5.1-1", -1),
            ("1.5-2", "1.5.1-1", -1),
            ("1.5-2", "1.5.1-2", -1),
            ("1.5", "1.5-1", 0),
            ("1.5-1", "1.5", 0),
            ("1.1-1", "1.1", 0),
            ("1.0-1", "1.1", -1),
            ("1.1-1", "1.0", 1),
            ("1.5b-1", "1.5-1", -1),
            ("1.5b", "1.5", -1),
            ("1.5b-1", "1.5", -1),
            ("1.5b", "1.5.1", -1),
            ("1.0a", "1.0alpha", -1),
            ("1.0alpha", "1.0b", -1),
            ("1.0b", "1.0beta", -1),
            ("1.0beta", "1.0rc", -1),
            ("1.0rc", "1.0", -1),
            ("1.5.a", "1.5", 1),
            ("1.5.b", "1.5.a", 1),
            ("1.5.1", "1.5.b", 1),
            ("1.5.b-1", "1.5.b", 0),
            ("1.5-1", "1.5.b", -1),
            ("2.0", "2_0", 0),
            ("2.0_a", "2_0.a", 0),
            ("2.0a", "2.0.a", -1),
            ("2___a", "2_a", 1),
            ("0:1.0", "0:1.0", 0),
            ("0:1.0", "0:1.1", -1),
            ("1:1.0", "0:1.0", 1),
            ("1:1.0", "0:1.1", 1),
            ("1:1.0", "2:1.1", -1),
            ("1:1.0", "0:1.0-1", 1),
            ("1:1.0-1", "0:1.1-1", 1),
            ("0:1.0", "1.0", 0),
            ("0:1.0", "1.1", -1),
            ("0:1.1", "1.0", 1),
            ("1:1.0", "1.0", 1),
            ("1:1.0", "1.1", 1),
            ("1:1.1", "1.1", 1),
        ];
        for &(a, b, expected) in cases {
            assert_eq!(expected, vercmp(a, b), "{a} vs {b}");
            assert_eq!(-expected, vercmp(b, a), "{b} vs {a}");
        }
    }
}
