//! Fitting rows into a byte-bounded page (R9).
//!
//! The listings (`list_documents`, `get_workspace`) used to add one row at a
//! time, rebuild the whole page (cloning every row so far) and serialise it
//! twice to find out whether it still fit: O(rows squared) clones and
//! serialisations. A page's size never shrinks when a row is added, so the
//! largest page that fits can be found by bisection, and a row-size lower bound
//! computed in one pass caps the search. Output is byte-identical to the row by
//! row scan: the same prefix, the same page.

/// How many leading rows fit. `row_sizes[i]` is the serialised size of row `i`
/// on its own; `page_bytes(k)` is the exact serialised size of the finished
/// page holding the first `k` rows (called only for `k >= 1`; zero rows are
/// taken to fit, as the row-by-row scan did).
pub(crate) fn largest_fitting_prefix(
    row_sizes: &[usize],
    max_bytes: usize,
    mut page_bytes: impl FnMut(usize) -> Result<usize, String>,
) -> Result<usize, String> {
    // A page holds its rows and the commas between them, so rows that do not
    // fit on their own cannot fit in a page.
    let mut upper = 0;
    let mut sum = 0usize;
    for (index, size) in row_sizes.iter().enumerate() {
        sum = sum.saturating_add(*size).saturating_add(usize::from(index > 0));
        if sum > max_bytes {
            break;
        }
        upper = index + 1;
    }
    let (mut low, mut high) = (0usize, upper);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if page_bytes(mid)? <= max_bytes {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    Ok(low)
}

/// Serialised size of each row, in one pass.
pub(crate) fn row_sizes(rows: &[serde_json::Value]) -> Result<Vec<usize>, String> {
    rows.iter()
        .map(|row| {
            serde_json::to_vec(row)
                .map(|bytes| bytes.len())
                .map_err(|error| format!("serialize page row: {error}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row-by-row scan this replaces, over a toy page of `{"rows":[..],"n":k}`.
    fn scan(sizes: &[usize], max: usize, envelope: impl Fn(usize) -> usize) -> usize {
        let mut fitted = 0;
        for k in 1..=sizes.len() {
            let bytes = envelope(k) + sizes[..k].iter().sum::<usize>() + (k - 1);
            if bytes > max {
                break;
            }
            fitted = k;
        }
        fitted
    }

    #[test]
    fn bisection_picks_the_same_prefix_as_the_row_by_row_scan() {
        // An envelope that grows with the digits of k, like a real page.
        let envelope = |k: usize| 40 + k.to_string().len() * 3;
        for count in [0usize, 1, 2, 7, 33, 100] {
            let sizes: Vec<usize> = (0..count).map(|i| 5 + (i * 7) % 23).collect();
            for max in [0usize, 10, 45, 60, 100, 333, 1_000, 10_000] {
                let expected = scan(&sizes, max, envelope);
                let mut calls = 0;
                let got = largest_fitting_prefix(&sizes, max, |k| {
                    calls += 1;
                    Ok(envelope(k) + sizes[..k].iter().sum::<usize>() + (k - 1))
                })
                .unwrap();
                assert_eq!(got, expected, "count {count}, max {max}");
                assert!(calls <= 8, "bisection, not a scan: {calls} evaluations");
            }
        }
    }

    #[test]
    fn rows_too_big_for_the_page_never_evaluate_it() {
        let got = largest_fitting_prefix(&[500, 5], 100, |_| panic!("not evaluated")).unwrap();
        assert_eq!(got, 0);
    }

    #[test]
    fn row_sizes_are_serialised_lengths() {
        let rows = vec![serde_json::json!({"a": 1}), serde_json::json!("xy")];
        assert_eq!(row_sizes(&rows).unwrap(), vec![7, 4]);
    }
}
