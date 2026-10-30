//! Static word embeddings (a token -> vector table) for explore's optional synonym
//! matching: a question word can match a code or doc word with a similar meaning.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result, bail};

/// Two words count as synonyms at or above this cosine similarity.
pub const SYNONYM: f32 = 0.6;
/// Words kept from a model file: they are usually sorted by frequency, and the tail is
/// rare words that cost memory and load time.
pub const MAX_WORDS: usize = 200_000;

pub struct Embeddings {
    dim: usize,
    /// Lowercase token -> row in `data`.
    rows: HashMap<String, u32>,
    /// Unit vectors, `dim` floats per row.
    data: Vec<f32>,
}

impl Embeddings {
    /// Reads the text format of word2vec, fastText and GloVe (`.vec`/`.txt`): an optional
    /// `count dim` header, then one `token v1 … vD` line per word (a token may contain
    /// spaces: the values are the last D fields). Lines that do not parse are skipped.
    /// Streams the file and keeps at most `MAX_WORDS` words.
    pub fn load(path: &Path) -> Result<Embeddings> {
        let file =
            File::open(path).with_context(|| format!("reading embeddings {}", path.display()))?;
        Self::read(BufReader::new(file)).with_context(|| format!("embeddings {}", path.display()))
    }

    pub fn read(reader: impl BufRead) -> Result<Embeddings> {
        let mut dim = 0;
        let mut rows = HashMap::new();
        let mut data = Vec::new();
        let mut first = true;
        for line in reader.lines() {
            let line = line?;
            let line = line.trim_start_matches('\u{feff}').trim_end();
            if line.trim().is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split(' ').filter(|f| !f.is_empty()).collect();
            if std::mem::take(&mut first)
                && fields.len() == 2
                && fields.iter().all(|f| f.parse::<usize>().is_ok())
            {
                dim = fields[1].parse()?;
                continue;
            }
            if dim == 0 {
                // No header: the first vector sets the dimension.
                dim = fields.len().saturating_sub(1);
            }
            if dim == 0 || fields.len() <= dim {
                continue;
            }
            let (token, values) = fields.split_at(fields.len() - dim);
            let Ok(values) = values
                .iter()
                .map(|v| v.parse::<f32>())
                .collect::<Result<Vec<_>, _>>()
            else {
                continue;
            };
            let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
            let token = token.join(" ").to_lowercase();
            if norm > 0.0 && !rows.contains_key(&token) {
                rows.insert(token, (data.len() / dim) as u32);
                data.extend(values.iter().map(|v| v / norm));
                if rows.len() == MAX_WORDS {
                    break;
                }
            }
        }
        if rows.is_empty() {
            bail!("no word vectors found");
        }
        Ok(Embeddings { dim, rows, data })
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    fn vector(&self, word: &str) -> Option<&[f32]> {
        let row = *self.rows.get(word)? as usize;
        Some(&self.data[row * self.dim..(row + 1) * self.dim])
    }

    /// Cosine similarity of two words, if both are known.
    pub fn similarity(&self, a: &str, b: &str) -> Option<f32> {
        let (x, y) = (self.vector(a)?, self.vector(b)?);
        Some(x.iter().zip(y).map(|(p, q)| p * q).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Embeddings> {
        Embeddings::read(text.as_bytes())
    }

    #[test]
    fn parses_and_compares() {
        let e = parse("3 2\nfetch 1 0\nRetrieve 0.9 0.1\nbanana 0 1\n").unwrap();
        assert_eq!((e.len(), e.dim()), (3, 2));
        assert!(e.similarity("fetch", "retrieve").unwrap() > 0.9);
        assert!(e.similarity("fetch", "banana").unwrap().abs() < 1e-6);
        assert!(e.similarity("fetch", "unknown").is_none());
    }

    #[test]
    fn tolerates_real_world_files() {
        // BOM and blank line before the header, a token with spaces, a broken line.
        let e = parse("\u{feff}\n4 2\n. . . 1 0\nok 0 1\nbroken x y\nshort 1\n").unwrap();
        assert_eq!(e.len(), 2);
        assert!(e.similarity(". . .", "ok").is_some());
        // No header: the first line sets the dimension.
        assert_eq!(parse("a 1 2 3\nb 3 2 1\n").unwrap().dim(), 3);
        assert!(parse("").is_err());
        assert!(parse("only words here\n").is_err());
    }
}
