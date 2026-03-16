use std::cmp::Ordering;
use std::fmt::{Display, Formatter};
use std::str::FromStr;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LooseVersion {
    raw: String,
    parts: Vec<u32>,
}

impl LooseVersion {
    pub fn parse(input: &str) -> Option<Self> {
        Self::from_str(input).ok()
    }
}

impl FromStr for LooseVersion {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let text = s.trim();
        if text.is_empty() {
            return Err("version is empty".to_owned());
        }
        let mut parts = Vec::new();
        for segment in text.split('.') {
            let parsed = segment
                .parse::<u32>()
                .map_err(|_| format!("invalid version segment: {segment}"))?;
            parts.push(parsed);
        }
        Ok(Self {
            raw: text.to_owned(),
            parts,
        })
    }
}

impl Ord for LooseVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let max_len = self.parts.len().max(other.parts.len());
        for idx in 0..max_len {
            let left = *self.parts.get(idx).unwrap_or(&0);
            let right = *other.parts.get(idx).unwrap_or(&0);
            match left.cmp(&right) {
                Ordering::Equal => continue,
                non_eq => return non_eq,
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for LooseVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Display for LooseVersion {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}
