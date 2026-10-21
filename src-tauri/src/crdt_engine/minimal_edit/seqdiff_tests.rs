//! Tests for `minimal_edit::seqdiff` (owner U3; DESIGN §4.3).

use super::seqdiff::{common_affixes, longest_increasing_pairs, myers_by, Edit};

fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

fn run(a: &[char], b: &[char], max_d: usize) -> Option<Vec<Edit>> {
    myers_by(a, b, |x, y| x == y, max_d)
}

/// Every string over {a, b} of length 0..=6.
fn all_strings() -> Vec<Vec<char>> {
    let mut out = vec![Vec::new()];
    let mut layer: Vec<Vec<char>> = vec![Vec::new()];
    for _ in 0..6 {
        let mut next = Vec::new();
        for base in &layer {
            for c in ['a', 'b'] {
                let mut s = base.clone();
                s.push(c);
                next.push(s);
            }
        }
        out.extend(next.iter().cloned());
        layer = next;
    }
    out
}

fn is_subsequence(needle: &[char], hay: &[char]) -> bool {
    let mut at = 0;
    for c in hay {
        if at < needle.len() && needle[at] == *c {
            at += 1;
        }
    }
    at == needle.len()
}

/// Longest common subsequence length by trying every subsequence of `a`.
fn brute_lcs(a: &[char], b: &[char]) -> usize {
    let mut best = 0;
    for mask in 0u32..(1 << a.len()) {
        let pick: Vec<char> = (0..a.len())
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| a[i])
            .collect();
        if pick.len() > best && is_subsequence(&pick, b) {
            best = pick.len();
        }
    }
    best
}

fn cost(script: &[Edit]) -> usize {
    script
        .iter()
        .map(|e| match e {
            Edit::Equal { .. } => 0,
            Edit::Delete { len, .. } | Edit::Insert { len, .. } => *len,
        })
        .sum()
}

/// Walk the script: it must consume `a` and `b` exactly, in order, Equal runs really equal, and no two
/// adjacent ops of the same kind.
fn assert_covers(script: &[Edit], a: &[char], b: &[char]) {
    let (mut ai, mut bi) = (0, 0);
    let mut last: Option<std::mem::Discriminant<Edit>> = None;
    for edit in script {
        let kind = std::mem::discriminant(edit);
        assert_ne!(last, Some(kind), "adjacent ops of one kind in {script:?}");
        last = Some(kind);
        match *edit {
            Edit::Equal { a: ea, b: eb, len } => {
                assert!(len > 0);
                assert_eq!((ea, eb), (ai, bi), "equal out of place in {script:?}");
                assert_eq!(a[ea..ea + len], b[eb..eb + len]);
                ai += len;
                bi += len;
            }
            Edit::Delete { a: ea, len } => {
                assert!(len > 0);
                assert_eq!(ea, ai, "delete out of place in {script:?}");
                ai += len;
            }
            Edit::Insert { b: eb, len } => {
                assert!(len > 0);
                assert_eq!(eb, bi, "insert out of place in {script:?}");
                bi += len;
            }
        }
    }
    assert_eq!((ai, bi), (a.len(), b.len()), "script leaves input unread");
}

#[test]
fn myers_is_minimal_against_brute_force() {
    let strings = all_strings();
    assert_eq!(strings.len(), 127);
    for a in &strings {
        for b in &strings {
            let script = run(a, b, usize::MAX / 4).expect("uncapped never fails");
            let want = a.len() + b.len() - 2 * brute_lcs(a, b);
            assert_eq!(cost(&script), want, "a={a:?} b={b:?} script={script:?}");
        }
    }
}

#[test]
fn script_covers_both_inputs() {
    let strings = all_strings();
    for a in &strings {
        for b in &strings {
            let script = run(a, b, 100).expect("within cap");
            assert_covers(&script, a, b);
        }
    }
    // Longer, structured inputs.
    let a = chars("the quick brown fox jumps over the lazy dog");
    let b = chars("a quick brown dog jumps over the lazy fox!");
    let script = run(&a, &b, 1000).expect("within cap");
    assert_covers(&script, &a, &b);
    // Empty sides.
    assert_eq!(run(&[], &[], 0), Some(vec![]));
    assert_eq!(
        run(&[], &chars("xy"), 5),
        Some(vec![Edit::Insert { b: 0, len: 2 }])
    );
    assert_eq!(
        run(&chars("xy"), &[], 5),
        Some(vec![Edit::Delete { a: 0, len: 2 }])
    );
}

#[test]
fn max_d_cutoff_returns_none() {
    let a = chars("abc");
    let b = chars("xyz");
    assert_eq!(run(&a, &b, 5), None);
    assert!(run(&a, &b, 6).is_some());
    // Equal inputs have distance 0, so even a zero cap succeeds.
    assert_eq!(
        run(&a, &a, 0),
        Some(vec![Edit::Equal { a: 0, b: 0, len: 3 }])
    );
    // One edit over a zero cap fails.
    assert_eq!(run(&chars("ab"), &chars("abc"), 0), None);
    assert!(run(&chars("ab"), &chars("abc"), 1).is_some());
}

#[test]
fn ties_put_delete_before_insert() {
    assert_eq!(
        run(&chars("a"), &chars("b"), 10),
        Some(vec![
            Edit::Delete { a: 0, len: 1 },
            Edit::Insert { b: 0, len: 1 }
        ])
    );
    // Over every pair, an Insert is never directly followed by a Delete.
    let strings = all_strings();
    for a in &strings {
        for b in &strings {
            let script = run(a, b, 100).expect("within cap");
            for pair in script.windows(2) {
                assert!(
                    !matches!(
                        (&pair[0], &pair[1]),
                        (Edit::Insert { .. }, Edit::Delete { .. })
                    ),
                    "insert before delete: a={a:?} b={b:?} {script:?}"
                );
            }
        }
    }
}

#[test]
fn lis_is_longest_and_deterministic() {
    // Brute force the longest strictly increasing chain over a small deterministic family of inputs.
    fn brute(pairs: &[(usize, usize)]) -> usize {
        let mut best = 0;
        for mask in 0u32..(1 << pairs.len()) {
            let pick: Vec<(usize, usize)> = (0..pairs.len())
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| pairs[i])
                .collect();
            let mut sorted = pick.clone();
            sorted.sort();
            let ok = sorted
                .windows(2)
                .all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1);
            if ok && sorted.len() > best {
                best = sorted.len();
            }
        }
        best
    }
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..200 {
        let len = (next() % 9) as usize;
        let pairs: Vec<(usize, usize)> = (0..len)
            .map(|_| ((next() % 6) as usize, (next() % 6) as usize))
            .collect();
        let chain = longest_increasing_pairs(&pairs);
        assert_eq!(
            chain.len(),
            brute(&pairs),
            "pairs={pairs:?} chain={chain:?}"
        );
        for w in chain.windows(2) {
            assert!(w[0].0 < w[1].0 && w[0].1 < w[1].1, "{chain:?}");
        }
        for p in &chain {
            assert!(pairs.contains(p));
        }
        // Input order is irrelevant.
        let mut reversed = pairs.clone();
        reversed.reverse();
        assert_eq!(longest_increasing_pairs(&reversed), chain);
        assert_eq!(longest_increasing_pairs(&pairs), chain);
    }
    // A moved block: 0->1, 1->2, 2->0 keeps the two that stay in order.
    assert_eq!(
        longest_increasing_pairs(&[(0, 1), (1, 2), (2, 0)]),
        vec![(0, 1), (1, 2)]
    );
    assert_eq!(longest_increasing_pairs(&[]), vec![]);
}

#[test]
fn affixes_do_not_overlap() {
    let eq = |x: &char, y: &char| x == y;
    assert_eq!(
        common_affixes(&chars("abcdef"), &chars("abXdef"), eq),
        (2, 3)
    );
    // "aa" vs "aaa": the prefix takes all of the shorter side, so the suffix is empty.
    assert_eq!(common_affixes(&chars("aa"), &chars("aaa"), eq), (2, 0));
    assert_eq!(common_affixes(&chars("aaa"), &chars("aa"), eq), (2, 0));
    assert_eq!(common_affixes(&chars("abab"), &chars("ab"), eq), (2, 0));
    assert_eq!(common_affixes(&chars("abc"), &chars("abc"), eq), (3, 0));
    assert_eq!(common_affixes(&chars(""), &chars("abc"), eq), (0, 0));
    assert_eq!(common_affixes(&chars("xa"), &chars("ya"), eq), (0, 1));
    // Exhaustive: prefix + suffix never exceeds the shorter side.
    let strings = all_strings();
    for a in &strings {
        for b in &strings {
            let (p, s) = common_affixes(a, b, eq);
            assert!(p + s <= a.len().min(b.len()), "a={a:?} b={b:?} {p} {s}");
            assert_eq!(a[..p], b[..p]);
            assert_eq!(a[a.len() - s..], b[b.len() - s..]);
        }
    }
}
