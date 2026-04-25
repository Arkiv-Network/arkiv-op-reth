//! Persisted entity record + serialization helpers.
//!
//! Entities are the units stored in the RocksDB-backed [`super::rocksdb_store::RocksDbStore`].
//! Each [`EntityRecord`] is the latest state for a given `entity_key`.

use alloy_primitives::{Address, B256, Bytes};
use serde::{Deserialize, Serialize};

use crate::storage::Annotation;

/// A persisted entity (latest version).
///
/// Mirrors the fields needed by the arkiv-sdk-js `RpcEntity` type
/// (see `src/types/rpcSchema.ts` in arkiv-sdk-js).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntityRecord {
    pub key: B256,
    pub owner: Address,
    pub creator: Address,
    pub expires_at: u64,
    pub created_at_block: u64,
    pub last_modified_at_block: u64,
    pub transaction_index_in_block: u32,
    pub operation_index_in_transaction: u32,
    pub content_type: String,
    pub payload: Bytes,
    /// String attributes (sorted by key).
    pub string_attributes: Vec<(String, String)>,
    /// Numeric attributes (sorted by key).
    pub numeric_attributes: Vec<(String, u64)>,
}

impl EntityRecord {
    /// Look up a string attribute by name.
    pub fn get_string(&self, name: &str) -> Option<&str> {
        self.string_attributes
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Look up a numeric attribute by name.
    pub fn get_numeric(&self, name: &str) -> Option<u64> {
        self.numeric_attributes
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| *v)
    }

    /// True if the entity carries an attribute with this name (string or numeric).
    pub fn has_attribute(&self, name: &str) -> bool {
        self.string_attributes.iter().any(|(k, _)| k == name)
            || self.numeric_attributes.iter().any(|(k, _)| k == name)
    }

    /// Build an [`EntityRecord`] from a list of [`Annotation`]s plus the other fields.
    /// Splits annotations into string/numeric vectors (sorted by key for deterministic output).
    pub fn from_parts(
        key: B256,
        owner: Address,
        creator: Address,
        expires_at: u64,
        created_at_block: u64,
        last_modified_at_block: u64,
        tx_index: u32,
        op_index: u32,
        content_type: String,
        payload: Bytes,
        annotations: &[Annotation],
    ) -> Self {
        let mut string_attributes: Vec<(String, String)> = Vec::new();
        let mut numeric_attributes: Vec<(String, u64)> = Vec::new();
        for ann in annotations {
            match ann {
                Annotation::String { key, string_value } => {
                    string_attributes.push((key.clone(), string_value.clone()));
                }
                Annotation::Numeric { key, numeric_value } => {
                    numeric_attributes.push((key.clone(), *numeric_value));
                }
            }
        }
        string_attributes.sort_by(|a, b| a.0.cmp(&b.0));
        numeric_attributes.sort_by(|a, b| a.0.cmp(&b.0));
        Self {
            key,
            owner,
            creator,
            expires_at,
            created_at_block,
            last_modified_at_block,
            transaction_index_in_block: tx_index,
            operation_index_in_transaction: op_index,
            content_type,
            payload,
            string_attributes,
            numeric_attributes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_sorts_attributes_and_separates_types() {
        let anns = vec![
            Annotation::String {
                key: "z".into(),
                string_value: "1".into(),
            },
            Annotation::Numeric {
                key: "b".into(),
                numeric_value: 2,
            },
            Annotation::String {
                key: "a".into(),
                string_value: "x".into(),
            },
        ];
        let e = EntityRecord::from_parts(
            B256::ZERO,
            Address::ZERO,
            Address::ZERO,
            0,
            0,
            0,
            0,
            0,
            "text/plain".into(),
            Bytes::new(),
            &anns,
        );
        assert_eq!(e.string_attributes, vec![("a".into(), "x".into()), ("z".into(), "1".into())]);
        assert_eq!(e.numeric_attributes, vec![("b".into(), 2)]);
        assert_eq!(e.get_string("a"), Some("x"));
        assert_eq!(e.get_numeric("b"), Some(2));
        assert!(e.has_attribute("z"));
        assert!(!e.has_attribute("missing"));
    }
}
