//! Sequence alignment primitives: Myers' O((n+m)·d) diff, LIS anchoring, common affixes. Pure, no yrs.

/// One edit of a Myers script. Indices are into the input slices. A script covers both inputs exactly,
/// in order, with adjacent ops of the same kind merged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Edit {
    Equal { a: usize, b: usize, len: usize },
    Delete { a: usize, len: usize },
    Insert { b: usize, len: usize },
}

/// Shortest edit script from `a` to `b` under `eq`, or `None` when the edit distance exceeds `max_d`.
/// Deterministic: on ties a Delete is emitted before an Insert.
pub(crate) fn myers_by<A, B, F>(a: &[A], b: &[B], eq: F, max_d: usize) -> Option<Vec<Edit>>
where
    F: Fn(&A, &B) -> bool,
{
    let n = a.len() as isize;
    let m = b.len() as isize;
    if n == 0 && m == 0 {
        return Some(Vec::new());
    }
    // The distance never exceeds n+m, so the cap needs no more room than that.
    let limit = (a.len() + b.len()).min(max_d) as isize;
    let offset = limit + 1;
    // v[k + offset] = furthest x reached on diagonal k (k = x - y).
    let mut v = vec![0isize; (2 * limit + 3) as usize];
    // trace[d][k + d] = v[k] after round d, for k in -d..=d (only these are ever read back).
    let mut trace: Vec<Vec<i32>> = Vec::new();
    let mut found: Option<isize> = None;

    'rounds: for d in 0..=limit {
        let mut k = -d;
        while k <= d {
            let down = k == -d || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]);
            let mut x = if down {
                v[(k + 1 + offset) as usize]
            } else {
                v[(k - 1 + offset) as usize] + 1
            };
            let mut y = x - k;
            while x < n && y < m && eq(&a[x as usize], &b[y as usize]) {
                x += 1;
                y += 1;
            }
            v[(k + offset) as usize] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'rounds;
            }
            k += 2;
        }
        let slice: Vec<i32> = (-d..=d)
            .map(|k| v[(k + offset) as usize] as i32)
            .collect();
        trace.push(slice);
    }
    let found = found?;

    // Backtrack to unit operations, newest first.
    enum Unit {
        Eq(usize, usize),
        Del(usize),
        Ins(usize),
    }
    let mut units: Vec<Unit> = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..=found).rev() {
        let k = x - y;
        let prev = &trace[(d - 1) as usize];
        let at = |kk: isize| prev[(kk + d - 1) as usize] as isize;
        let down = k == -d || (k != d && at(k - 1) < at(k + 1));
        let pk = if down { k + 1 } else { k - 1 };
        let px = at(pk);
        let py = px - pk;
        let (mx, my) = if down { (px, py + 1) } else { (px + 1, py) };
        while x > mx && y > my {
            x -= 1;
            y -= 1;
            units.push(Unit::Eq(x as usize, y as usize));
        }
        if down {
            units.push(Unit::Ins(py as usize));
        } else {
            units.push(Unit::Del(px as usize));
        }
        x = px;
        y = py;
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        units.push(Unit::Eq(x as usize, y as usize));
    }
    units.reverse();

    // Assemble runs. Between two Equal runs, every Delete goes before every Insert (the tie rule).
    let mut script: Vec<Edit> = Vec::new();
    let mut eq_run: Option<(usize, usize, usize)> = None;
    let mut del_run: Option<(usize, usize)> = None;
    let mut ins_run: Option<(usize, usize)> = None;
    fn flush_changes(
        script: &mut Vec<Edit>,
        del_run: &mut Option<(usize, usize)>,
        ins_run: &mut Option<(usize, usize)>,
    ) {
        if let Some((a, len)) = del_run.take() {
            script.push(Edit::Delete { a, len });
        }
        if let Some((b, len)) = ins_run.take() {
            script.push(Edit::Insert { b, len });
        }
    }
    for unit in units {
        match unit {
            Unit::Eq(ai, bi) => {
                flush_changes(&mut script, &mut del_run, &mut ins_run);
                match &mut eq_run {
                    Some((_, _, len)) => *len += 1,
                    None => eq_run = Some((ai, bi, 1)),
                }
            }
            Unit::Del(ai) => {
                if let Some((a, b, len)) = eq_run.take() {
                    script.push(Edit::Equal { a, b, len });
                }
                match &mut del_run {
                    Some((_, len)) => *len += 1,
                    None => del_run = Some((ai, 1)),
                }
            }
            Unit::Ins(bi) => {
                if let Some((a, b, len)) = eq_run.take() {
                    script.push(Edit::Equal { a, b, len });
                }
                match &mut ins_run {
                    Some((_, len)) => *len += 1,
                    None => ins_run = Some((bi, 1)),
                }
            }
        }
    }
    flush_changes(&mut script, &mut del_run, &mut ins_run);
    if let Some((a, b, len)) = eq_run.take() {
        script.push(Edit::Equal { a, b, len });
    }
    Some(script)
}

/// Longest common prefix and suffix lengths (suffix never overlaps the prefix).
pub(crate) fn common_affixes<A, B, F>(a: &[A], b: &[B], eq: F) -> (usize, usize)
where
    F: Fn(&A, &B) -> bool,
{
    let shortest = a.len().min(b.len());
    let mut prefix = 0;
    while prefix < shortest && eq(&a[prefix], &b[prefix]) {
        prefix += 1;
    }
    let mut suffix = 0;
    while prefix + suffix < shortest && eq(&a[a.len() - 1 - suffix], &b[b.len() - 1 - suffix]) {
        suffix += 1;
    }
    (prefix, suffix)
}

/// The longest chain of `pairs` strictly increasing in both coordinates. Input order is irrelevant; output
/// is sorted by `.0`. Ties broken by the smaller `.1`, then by input order (deterministic). O(k log k).
pub(crate) fn longest_increasing_pairs(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut sorted: Vec<(usize, usize)> = pairs.to_vec();
    // Same `.0`: larger `.1` first, so a strictly increasing `.1` chain can take at most one of them.
    // The sort is stable, so equal pairs keep their input order.
    sorted.sort_by(|x, y| x.0.cmp(&y.0).then(y.1.cmp(&x.1)));
    // Patience sorting: tails[len-1] = index of the smallest `.1` ending a chain of that length.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; sorted.len()];
    for (i, pair) in sorted.iter().enumerate() {
        let pos = tails.partition_point(|&t| sorted[t].1 < pair.1);
        prev[i] = if pos > 0 { Some(tails[pos - 1]) } else { None };
        if pos == tails.len() {
            tails.push(i);
        } else {
            tails[pos] = i;
        }
    }
    let mut chain: Vec<(usize, usize)> = Vec::with_capacity(tails.len());
    let mut at = tails.last().copied();
    while let Some(i) = at {
        chain.push(sorted[i]);
        at = prev[i];
    }
    chain.reverse();
    chain
}
