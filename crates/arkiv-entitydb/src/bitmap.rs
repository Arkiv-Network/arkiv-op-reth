//! Roaring64 bitmap of entity IDs.
//!
//! The canonical wire format is the portable `RoaringFormatSpec`
//! layout (see [`Bitmap::to_bytes`]). [`Bitmap::to_bytes`] is
//! deterministic — same set of IDs → same bytes — which is what makes
//! `codeHash = keccak256(bitmap_bytes)` agree across nodes for pair
//! accounts.

use eyre::Result;
use roaring::RoaringTreemap;

/// Roaring64 bitmap of entity IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bitmap(RoaringTreemap);

impl Bitmap {
    pub fn new() -> Self {
        Self(RoaringTreemap::new())
    }

    /// Deserialize from the portable RoaringFormatSpec layout.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        RoaringTreemap::deserialize_from(bytes)
            .map(Self)
            .map_err(|e| eyre::eyre!("invalid roaring bitmap bytes: {e}"))
    }

    /// Serialize to the portable RoaringFormatSpec layout. Same set →
    /// same bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.0.serialized_size());
        self.0
            .serialize_into(&mut buf)
            .expect("writing to Vec is infallible");
        buf
    }

    pub fn insert(&mut self, id: u64) -> bool {
        self.0.insert(id)
    }

    pub fn remove(&mut self, id: u64) -> bool {
        self.0.remove(id)
    }

    pub fn contains(&self, id: u64) -> bool {
        self.0.contains(id)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> u64 {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter()
    }

    /// In-place set union: `self ∪= other`.
    pub fn union_with(&mut self, other: &Bitmap) {
        self.0 |= &other.0;
    }

    /// In-place set intersection: `self ∩= other`.
    pub fn intersect_with(&mut self, other: &Bitmap) {
        self.0 &= &other.0;
    }

    /// In-place set difference: `self \= other`.
    pub fn subtract(&mut self, other: &Bitmap) {
        self.0 -= &other.0;
    }
}

impl FromIterator<u64> for Bitmap {
    fn from_iter<I: IntoIterator<Item = u64>>(iter: I) -> Self {
        Self(RoaringTreemap::from_iter(iter))
    }
}
