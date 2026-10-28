use std::collections::HashMap;

pub const MAX_ENTRIES: usize = 255;

#[derive(Debug, Clone, Default)]
pub struct StringTable {
    entries: Vec<Vec<u8>>,
    rev: HashMap<Vec<u8>, u8>,
}

impl StringTable {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn parse(data: &[u8]) -> Self {
        let mut entries: Vec<Vec<u8>> = Vec::new();
        for line in data.split(|&b| b == b'\n') {
            let line = match line.split_last() {
                Some((b'\r', head)) => head,
                _ => line,
            };
            entries.push(line.to_vec());
        }
        if entries.last().is_some_and(|l| l.is_empty()) {
            entries.pop();
        }

        let mut rev = HashMap::with_capacity(entries.len());
        for (i, entry) in entries.iter().enumerate() {
            if i >= MAX_ENTRIES {
                break;
            }
            rev.entry(entry.clone()).or_insert((i + 1) as u8);
        }
        StringTable { entries, rev }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, index: u8) -> Option<&[u8]> {
        if index == 0 {
            return None;
        }
        self.entries.get(index as usize - 1).map(|v| v.as_slice())
    }

    pub fn index_of(&self, s: &[u8]) -> Option<u8> {
        self.rev.get(s).copied()
    }
}
