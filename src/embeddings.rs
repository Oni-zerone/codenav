//! Static word embeddings (a token -> vector table) for explore's optional semantic
//! matching: a question word can match a code or doc word with a similar meaning.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// Two words count as synonyms at or above this cosine similarity.
pub const SYNONYM: f32 = 0.6;

pub struct Embeddings {
    dim: usize,
    /// Lowercase token -> unit vector.
    vectors: HashMap<String, Vec<f32>>,
}

impl Embeddings {
    /// Reads the text format of word2vec, fastText and GloVe (`.vec`/`.txt`): an optional
    /// `count dim` header, then one `token v1 … vD` line per token. Vectors are normalised.
    pub fn load(path: &Path) -> Result<Embeddings> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading embeddings {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("embeddings {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Embeddings> {
        let mut dim = 0;
        let mut vectors = HashMap::new();
        for (n, line) in text.lines().enumerate() {
            let mut parts = line.split_whitespace();
            let Some(token) = parts.next() else { continue };
            let values: Vec<f32> = parts
                .map(str::parse)
                .collect::<Result<_, _>>()
                .with_context(|| format!("line {}: not a token followed by numbers", n + 1))?;
            // `count dim` header.
            if n == 0 && values.len() == 1 && token.parse::<usize>().is_ok() {
                continue;
            }
            if values.is_empty() {
                continue;
            }
            if dim == 0 {
                dim = values.len();
            } else if values.len() != dim {
                bail!("line {}: {} values, expected {dim}", n + 1, values.len());
            }
            let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 0.0 {
                let unit = values.iter().map(|v| v / norm).collect();
                vectors.entry(token.to_lowercase()).or_insert(unit);
            }
        }
        if vectors.is_empty() {
            bail!("no vectors found");
        }
        Ok(Embeddings { dim, vectors })
    }

    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Cosine similarity of two words, if both are known.
    pub fn similarity(&self, a: &str, b: &str) -> Option<f32> {
        let (x, y) = (self.vectors.get(a)?, self.vectors.get(b)?);
        Some(x.iter().zip(y).map(|(p, q)| p * q).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_compares() {
        let e = Embeddings::parse("3 2\nfetch 1 0\nRetrieve 0.9 0.1\nbanana 0 1\n").unwrap();
        assert_eq!((e.len(), e.dim()), (3, 2));
        assert!(e.similarity("fetch", "retrieve").unwrap() > 0.9);
        assert!(e.similarity("fetch", "banana").unwrap().abs() < 1e-6);
        assert!(e.similarity("fetch", "unknown").is_none());
        assert!(Embeddings::parse("a 1 2\nb 1\n").is_err());
        assert!(Embeddings::parse("a x y\n").is_err());
        assert!(Embeddings::parse("").is_err());
    }
}
