use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

const SEMANTIC_INDEX_MIN_WORDS: usize = 3;
const SEMANTIC_INDEX_MAX_TEXT_CHARS: usize = 2_000;

pub(crate) fn semantic_text_is_indexable(value: &str) -> bool {
    value.split_whitespace().count() >= SEMANTIC_INDEX_MIN_WORDS
}

pub(crate) fn normalize_semantic_text(value: &str) -> String {
    truncate_chars(
        &value.split_whitespace().collect::<Vec<_>>().join(" "),
        SEMANTIC_INDEX_MAX_TEXT_CHARS,
    )
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

pub(crate) fn semantic_content_hash(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub(crate) fn normalize_vector(vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm <= f32::EPSILON {
        return vector;
    }
    vector.into_iter().map(|value| value / norm).collect()
}

pub(crate) fn dot_product(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right.iter())
        .map(|(left, right)| left * right)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_text_normalization_collapses_and_truncates() {
        let source = format!("  alpha\n beta\t{}\n", "z".repeat(3_000));
        let normalized = normalize_semantic_text(&source);

        assert!(normalized.starts_with("alpha beta "));
        assert_eq!(normalized.chars().count(), SEMANTIC_INDEX_MAX_TEXT_CHARS);
    }

    #[test]
    fn semantic_indexable_text_requires_minimum_words() {
        assert!(!semantic_text_is_indexable("one two"));
        assert!(semantic_text_is_indexable("one two three"));
    }

    #[test]
    fn semantic_vector_normalization_preserves_direction_unit_length() {
        let vector = normalize_vector(vec![3.0, 4.0]);
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();

        assert!((norm - 1.0).abs() < 0.0001);
        assert!((dot_product(&vector, &vector) - 1.0).abs() < 0.0001);
    }
}
