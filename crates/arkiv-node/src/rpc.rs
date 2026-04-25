//! JSON-RPC server exposing the Arkiv query API.
//!
//! Implements the methods consumed by the `@arkiv-network/sdk`
//! (`arkiv_query`, `arkiv_getEntity`, `arkiv_getEntityCount`,
//! `arkiv_getBlockTiming`).
//!
//! The server is started by `main.rs` whenever the `ARKIV_RPC_BIND`
//! environment variable is set (e.g. `127.0.0.1:8546`).

use std::cmp::Ordering;
use std::net::SocketAddr;
use std::sync::Arc;

use alloy_primitives::{Address, B256};
use jsonrpsee::server::{ServerBuilder, ServerHandle};
use jsonrpsee::types::ErrorObjectOwned;
use jsonrpsee::{RpcModule, core::async_trait, proc_macros::rpc};
use serde::{Deserialize, Serialize};

use crate::query::{
    Expr, extract_creator_constraint, extract_key_constraint, extract_owner_constraint, parser,
};
use crate::storage::entity::EntityRecord;
use crate::storage::rocksdb_store::RocksDbStore;

const DEFAULT_PAGE_SIZE: usize = 100;
const MAX_PAGE_SIZE: usize = 1000;

// ---- Wire types --------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcIncludeData {
    #[serde(default)]
    pub key: bool,
    #[serde(default)]
    pub attributes: bool,
    #[serde(default)]
    pub payload: bool,
    #[serde(default)]
    pub content_type: bool,
    #[serde(default)]
    pub expiration: bool,
    #[serde(default)]
    pub owner: bool,
    #[serde(default)]
    pub creator: bool,
    #[serde(default)]
    pub created_at_block: bool,
    #[serde(default)]
    pub last_modified_at_block: bool,
    #[serde(default)]
    pub transaction_index_in_block: bool,
    #[serde(default)]
    pub operation_index_in_transaction: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcOrderByAttribute {
    pub name: String,
    /// `"string"` or `"numeric"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub desc: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcQueryOptions {
    /// Hex u64 — the block number at which to evaluate the query.
    /// Currently only the latest state is supported; the field is accepted but ignored.
    #[allow(dead_code)]
    pub at_block: Option<String>,
    pub include_data: Option<RpcIncludeData>,
    pub order_by: Option<Vec<RpcOrderByAttribute>>,
    /// Hex u64 — number of results per page.
    pub results_per_page: Option<String>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcAttribute {
    pub key: String,
    pub value: String,
}

/// `RpcEntity` mirroring the `arkiv-sdk-js` `RpcEntity` type. Fields are
/// emitted only when requested via the `includeData` flags so that
/// `JSON.parse` clients receive the same shape as the upstream service.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RpcEntity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<B256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Hex-encoded payload (`0x...`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at_block: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified_at_block: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_index_in_block: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_index_in_transaction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub string_attributes: Option<Vec<RpcAttribute>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub numeric_attributes: Option<Vec<RpcAttribute>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcQueryResult {
    pub data: Vec<RpcEntity>,
    /// Hex u64.
    pub block_number: String,
    pub cursor: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct RpcBlockTiming {
    pub current_block: u64,
    pub current_block_time: u64,
    pub duration: u64,
}

// ---- RPC trait & impl --------------------------------------------------------

#[rpc(server, namespace = "arkiv")]
pub trait ArkivRpc {
    #[method(name = "query")]
    async fn query(
        &self,
        query: String,
        options: Option<RpcQueryOptions>,
    ) -> Result<RpcQueryResult, ErrorObjectOwned>;

    #[method(name = "getEntity")]
    async fn get_entity(&self, key: B256) -> Result<RpcEntity, ErrorObjectOwned>;

    #[method(name = "getEntityCount")]
    async fn get_entity_count(&self) -> Result<u64, ErrorObjectOwned>;

    #[method(name = "getBlockTiming")]
    async fn get_block_timing(&self) -> Result<RpcBlockTiming, ErrorObjectOwned>;
}

pub struct ArkivRpcImpl {
    store: Arc<RocksDbStore>,
}

impl ArkivRpcImpl {
    pub fn new(store: Arc<RocksDbStore>) -> Self {
        Self { store }
    }
}

fn rpc_err(msg: impl ToString) -> ErrorObjectOwned {
    ErrorObjectOwned::owned::<()>(-32000, msg.to_string(), None)
}

fn parse_hex_u64(s: &str) -> Result<u64, ErrorObjectOwned> {
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u64::from_str_radix(s, 16).map_err(|e| rpc_err(format!("invalid hex u64 '{}': {}", s, e)))
}

fn render_entity(entity: &EntityRecord, include: &RpcIncludeData) -> RpcEntity {
    let mut out = RpcEntity::default();
    if include.key {
        out.key = Some(entity.key);
    }
    if include.content_type {
        out.content_type = Some(entity.content_type.clone());
    }
    if include.payload {
        out.value = Some(format!("0x{}", hex::encode(entity.payload.as_ref())));
    }
    if include.expiration {
        out.expires_at = Some(format!("0x{:x}", entity.expires_at));
    }
    if include.created_at_block {
        out.created_at_block = Some(format!("0x{:x}", entity.created_at_block));
    }
    if include.last_modified_at_block {
        out.last_modified_at_block = Some(format!("0x{:x}", entity.last_modified_at_block));
    }
    if include.transaction_index_in_block {
        out.transaction_index_in_block =
            Some(format!("0x{:x}", entity.transaction_index_in_block));
    }
    if include.operation_index_in_transaction {
        out.operation_index_in_transaction =
            Some(format!("0x{:x}", entity.operation_index_in_transaction));
    }
    if include.owner {
        out.owner = Some(entity.owner);
    }
    if include.creator {
        out.creator = Some(entity.creator);
    }
    if include.attributes {
        out.string_attributes = Some(
            entity
                .string_attributes
                .iter()
                .map(|(k, v)| RpcAttribute { key: k.clone(), value: v.clone() })
                .collect(),
        );
        out.numeric_attributes = Some(
            entity
                .numeric_attributes
                .iter()
                .map(|(k, v)| RpcAttribute { key: k.clone(), value: format!("0x{:x}", v) })
                .collect(),
        );
    }
    out
}

/// Run the planner: pick a candidate set of entities given the parsed expression,
/// then filter with the full predicate.
fn plan_and_collect(store: &RocksDbStore, expr: &Expr) -> Result<Vec<EntityRecord>, ErrorObjectOwned> {
    let candidates: Vec<EntityRecord> = if let Some(key) = extract_key_constraint(expr) {
        match store.get_entity(&key).map_err(rpc_err)? {
            Some(e) => vec![e],
            None => Vec::new(),
        }
    } else if let Some(owner) = extract_owner_constraint(expr) {
        store.iter_entities_by_owner(&owner).map_err(rpc_err)?
    } else if let Some(creator) = extract_creator_constraint(expr) {
        store.iter_entities_by_creator(&creator).map_err(rpc_err)?
    } else {
        store.iter_entities().map_err(rpc_err)?
    };

    Ok(candidates.into_iter().filter(|e| expr.matches(e)).collect())
}

fn apply_order_by(entities: &mut [EntityRecord], order: &[RpcOrderByAttribute]) {
    if order.is_empty() {
        return;
    }
    entities.sort_by(|a, b| {
        for ob in order {
            let ord = match ob.kind.as_str() {
                "numeric" => match (a.get_numeric(&ob.name), b.get_numeric(&ob.name)) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => Ordering::Equal,
                },
                _ => match (a.get_string(&ob.name), b.get_string(&ob.name)) {
                    (Some(x), Some(y)) => x.cmp(y),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => Ordering::Equal,
                },
            };
            let ord = if ob.desc { ord.reverse() } else { ord };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        a.key.cmp(&b.key)
    });
}

#[async_trait]
impl ArkivRpcServer for ArkivRpcImpl {
    async fn query(
        &self,
        query: String,
        options: Option<RpcQueryOptions>,
    ) -> Result<RpcQueryResult, ErrorObjectOwned> {
        let opts = options.unwrap_or_default();
        let include = opts.include_data.clone().unwrap_or_default();

        let expr = parser::parse(&query).map_err(rpc_err)?;
        let store = Arc::clone(&self.store);

        let mut entities = tokio::task::spawn_blocking(move || plan_and_collect(&store, &expr))
            .await
            .map_err(rpc_err)??;

        if let Some(order) = &opts.order_by {
            apply_order_by(&mut entities, order);
        } else {
            entities.sort_by(|a, b| a.key.cmp(&b.key));
        }

        // Cursor is the entity_key (hex) of the last item from the previous page.
        if let Some(cursor) = &opts.cursor {
            let cursor = cursor.trim();
            if !cursor.is_empty() {
                let bytes = hex::decode(cursor.strip_prefix("0x").unwrap_or(cursor))
                    .map_err(|e| rpc_err(format!("invalid cursor hex: {}", e)))?;
                if bytes.len() != 32 {
                    return Err(rpc_err("cursor must be a 32-byte entity key"));
                }
                let cursor_key = B256::from_slice(&bytes);
                // Drop everything up to and including the cursor entry, by key order.
                let pos = entities.iter().position(|e| e.key > cursor_key);
                entities = match pos {
                    Some(p) => entities.split_off(p),
                    None => Vec::new(),
                };
            }
        }

        let limit = match opts.results_per_page.as_deref() {
            Some(s) => parse_hex_u64(s)? as usize,
            None => DEFAULT_PAGE_SIZE,
        }
        .clamp(1, MAX_PAGE_SIZE);

        let truncated: Vec<EntityRecord> = entities.into_iter().take(limit).collect();
        let next_cursor = truncated
            .last()
            .map(|e| format!("0x{}", hex::encode(e.key.as_slice())))
            .unwrap_or_default();
        let block_number = self
            .store
            .head_block_number()
            .map_err(rpc_err)?
            .unwrap_or(0);

        Ok(RpcQueryResult {
            data: truncated.iter().map(|e| render_entity(e, &include)).collect(),
            block_number: format!("0x{:x}", block_number),
            cursor: next_cursor,
        })
    }

    async fn get_entity(&self, key: B256) -> Result<RpcEntity, ErrorObjectOwned> {
        let store = Arc::clone(&self.store);
        let entity = tokio::task::spawn_blocking(move || store.get_entity(&key))
            .await
            .map_err(rpc_err)?
            .map_err(rpc_err)?
            .ok_or_else(|| rpc_err("entity not found"))?;
        // Default getEntity returns *all* fields (matches sdk's getEntity helper).
        let include = RpcIncludeData {
            key: true,
            attributes: true,
            payload: true,
            content_type: true,
            expiration: true,
            owner: true,
            creator: true,
            created_at_block: true,
            last_modified_at_block: true,
            transaction_index_in_block: true,
            operation_index_in_transaction: true,
        };
        Ok(render_entity(&entity, &include))
    }

    async fn get_entity_count(&self) -> Result<u64, ErrorObjectOwned> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.entity_count())
            .await
            .map_err(rpc_err)?
            .map_err(rpc_err)
    }

    async fn get_block_timing(&self) -> Result<RpcBlockTiming, ErrorObjectOwned> {
        let current_block = self.store.head_block_number().map_err(rpc_err)?.unwrap_or(0);
        Ok(RpcBlockTiming {
            current_block,
            current_block_time: 0,
            duration: 0,
        })
    }
}

// ---- Server bootstrap --------------------------------------------------------

/// Spawn the JSON-RPC server bound to `addr`, returning the server handle.
pub async fn spawn(store: Arc<RocksDbStore>, addr: SocketAddr) -> eyre::Result<ServerHandle> {
    let server = ServerBuilder::default().build(addr).await?;
    let mut module = RpcModule::new(());
    module.merge(ArkivRpcImpl::new(store).into_rpc())?;
    let handle = server.start(module);
    Ok(handle)
}

// (no unused-dep helpers needed)

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;

    fn mk_record(key: u8, owner: u8, attrs: Vec<(String, String)>, nums: Vec<(String, u64)>) -> EntityRecord {
        EntityRecord {
            key: B256::from_slice(&[key; 32]),
            owner: Address::from_slice(&[owner; 20]),
            creator: Address::from_slice(&[owner; 20]),
            expires_at: 100,
            created_at_block: 1,
            last_modified_at_block: 1,
            transaction_index_in_block: 0,
            operation_index_in_transaction: 0,
            content_type: "text/plain".into(),
            payload: Bytes::new(),
            string_attributes: attrs,
            numeric_attributes: nums,
        }
    }

    #[test]
    fn order_by_string_asc() {
        let mut v = vec![
            mk_record(2, 0, vec![("name".into(), "B".into())], vec![]),
            mk_record(1, 0, vec![("name".into(), "A".into())], vec![]),
            mk_record(3, 0, vec![("name".into(), "C".into())], vec![]),
        ];
        apply_order_by(
            &mut v,
            &[RpcOrderByAttribute { name: "name".into(), kind: "string".into(), desc: false }],
        );
        assert_eq!(v[0].get_string("name"), Some("A"));
        assert_eq!(v[2].get_string("name"), Some("C"));
    }

    #[test]
    fn order_by_numeric_desc() {
        let mut v = vec![
            mk_record(1, 0, vec![], vec![("score".into(), 5)]),
            mk_record(2, 0, vec![], vec![("score".into(), 10)]),
            mk_record(3, 0, vec![], vec![("score".into(), 3)]),
        ];
        apply_order_by(
            &mut v,
            &[RpcOrderByAttribute { name: "score".into(), kind: "numeric".into(), desc: true }],
        );
        assert_eq!(v[0].get_numeric("score"), Some(10));
        assert_eq!(v[2].get_numeric("score"), Some(3));
    }

    #[test]
    fn render_respects_include_flags() {
        let e = mk_record(1, 2, vec![("k".into(), "v".into())], vec![]);
        let mut inc = RpcIncludeData::default();
        inc.key = true;
        inc.payload = true;
        let r = render_entity(&e, &inc);
        assert!(r.key.is_some());
        assert_eq!(r.value.as_deref(), Some("0x"));
        assert!(r.owner.is_none());
        assert!(r.string_attributes.is_none());

        inc.attributes = true;
        let r = render_entity(&e, &inc);
        assert_eq!(r.string_attributes.as_ref().unwrap().len(), 1);
    }
}
