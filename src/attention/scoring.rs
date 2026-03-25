//! Vector arithmetic and cosine similarity scoring.
//!
//! Implements:
//! - Receptor vector computation: pos_centroid - NEGATIVE_WEIGHT * neg_centroid
//! - Cosine similarity for message scoring

/// Weight for negative example subtraction in receptor vector computation.
/// As per spec: 0.35
pub const NEGATIVE_WEIGHT: f32 = 0.35;

/// Compute the centroid (mean) of a set of vectors.
pub fn mean_vector(vectors: &[Vec<f32>]) -> Option<Vec<f32>> {
    if vectors.is_empty() {
        return None;
    }

    let dim = vectors[0].len();
    if dim == 0 {
        return None;
    }

    let mut result = vec![0.0f32; dim];
    let n = vectors.len() as f32;

    for vec in vectors {
        if vec.len() != dim {
            return None; // Dimension mismatch
        }
        for (i, &v) in vec.iter().enumerate() {
            result[i] += v;
        }
    }

    for v in &mut result {
        *v /= n;
    }

    Some(result)
}

/// Subtract weighted vector b from vector a: a - weight * b
pub fn subtract_weighted(a: &[f32], b: &[f32], weight: f32) -> Option<Vec<f32>> {
    if a.len() != b.len() {
        return None;
    }

    let result: Vec<f32> = a
        .iter()
        .zip(b.iter())
        .map(|(&ai, &bi)| ai - weight * bi)
        .collect();

    Some(result)
}

/// Compute cosine similarity between two vectors.
/// Returns 0.0 if either vector has zero magnitude.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let dot: f32 = a.iter().zip(b.iter()).map(|(&ai, &bi)| ai * bi).sum();
    let norm_a: f32 = a.iter().map(|&x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|&x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot / (norm_a * norm_b)
}

/// Compute a receptor vector from positive and negative example embeddings.
///
/// Formula: pos_centroid - NEGATIVE_WEIGHT * neg_centroid
///
/// Returns None if there are no positive vectors.
pub fn compute_receptor_vector(
    positive_vectors: &[Vec<f32>],
    negative_vectors: &[Vec<f32>],
) -> Option<Vec<f32>> {
    let pos_centroid = mean_vector(positive_vectors)?;

    if negative_vectors.is_empty() {
        return Some(pos_centroid);
    }

    let neg_centroid = mean_vector(negative_vectors)?;
    subtract_weighted(&pos_centroid, &neg_centroid, NEGATIVE_WEIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_mean_vector() {
        let vecs = vec![vec![1.0, 2.0, 3.0], vec![3.0, 4.0, 5.0]];
        let mean = mean_vector(&vecs).unwrap();
        assert_eq!(mean, vec![2.0, 3.0, 4.0]);
    }

    #[test]
    fn mean_of_single_vector_is_itself() {
        let vecs = vec![vec![1.0, 2.0, 3.0]];
        let mean = mean_vector(&vecs).unwrap();
        assert_eq!(mean, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn mean_of_empty_is_none() {
        let vecs: Vec<Vec<f32>> = vec![];
        assert!(mean_vector(&vecs).is_none());
    }

    #[test]
    fn subtracts_weighted_vector() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![2.0, 2.0, 2.0];
        let result = subtract_weighted(&a, &b, 0.5).unwrap();
        assert_eq!(result, vec![0.0, 1.0, 2.0]);
    }

    #[test]
    fn computes_cosine_similarity() {
        // Identical vectors should have similarity 1.0
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6);

        // Orthogonal vectors should have similarity 0.0
        let c = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &c);
        assert!((sim - 0.0).abs() < 1e-6);

        // Opposite vectors should have similarity -1.0
        let d = vec![-1.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &d);
        assert!((sim - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn computes_receptor_vector_without_negatives() {
        let pos = vec![vec![1.0, 2.0], vec![3.0, 4.0]];
        let neg: Vec<Vec<f32>> = vec![];
        let result = compute_receptor_vector(&pos, &neg).unwrap();
        assert_eq!(result, vec![2.0, 3.0]); // mean of positives
    }

    #[test]
    fn computes_receptor_vector_with_negatives() {
        let pos = vec![vec![1.0, 1.0]];
        let neg = vec![vec![1.0, 0.0]];
        // result = [1, 1] - 0.35 * [1, 0] = [0.65, 1.0]
        let result = compute_receptor_vector(&pos, &neg).unwrap();
        assert!((result[0] - 0.65).abs() < 1e-6);
        assert!((result[1] - 1.0).abs() < 1e-6);
    }
}
