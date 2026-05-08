//! `arkiv_*` JSON-RPC namespace.
//!
//! Two kinds of handlers:
//!
//! - **EntityDB proxies** (`arkiv_query`, `arkiv_getEntityCount`): forward
//!   positional args verbatim to the configured EntityDB and return the raw
//!   `result`. Share the same [`EntityDbClient`] and connection pool as the
//!   ExEx write-side [`JsonRpcStore`].
//! - **Node-local** (`arkiv_getBlockTiming`): answered entirely from the
//!   node's own chain state via [`BlockTimingSource`]; no EntityDB involved.

use crate::storage::EntityDbClient;
use jsonrpsee::core::{RpcResult, async_trait};
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

/// Block timing for the current chain head, computed from local chain state.
///
/// JSON shape matches the op-geth `BlockTiming` struct.
#[derive(Debug, Clone, Serialize)]
pub struct BlockTimingResponse {
    pub current_block: u64,
    pub current_block_time: u64,
    pub duration: u64,
}

/// Provides block timing derived from the node's own chain state.
///
/// Abstracts over the concrete reth provider type so `ArkivRpc` stays
/// non-generic. Implemented by `RethBlockTiming` in `install.rs`.
pub trait BlockTimingSource: Send + Sync + 'static {
    fn block_timing(&self) -> eyre::Result<BlockTimingResponse>;
}

#[rpc(server, namespace = "arkiv")]
pub trait ArkivApi {
    /// Query entities. `expr` is the EntityDB query expression; `options`
    /// is an optional object (paging / projection / atBlock).
    #[method(name = "query")]
    async fn query(&self, expr: String, options: Option<Value>) -> RpcResult<Value>;

    /// Total entity count currently stored in EntityDB.
    #[method(name = "getEntityCount")]
    async fn get_entity_count(&self) -> RpcResult<Value>;

    /// Current block number, timestamp, and seconds since the previous block.
    /// Computed from local chain state — does not proxy to EntityDB.
    #[method(name = "getBlockTiming")]
    async fn get_block_timing(&self) -> RpcResult<BlockTimingResponse>;
}

pub struct ArkivRpc {
    client: Arc<EntityDbClient>,
    timing: Arc<dyn BlockTimingSource>,
}

impl ArkivRpc {
    pub fn new(client: Arc<EntityDbClient>, timing: Arc<dyn BlockTimingSource>) -> Self {
        Self { client, timing }
    }
}

#[async_trait]
impl ArkivApiServer for ArkivRpc {
    async fn query(&self, expr: String, options: Option<Value>) -> RpcResult<Value> {
        let opts = options.unwrap_or(Value::Null);
        self.client
            .proxy("arkiv_query", vec![Value::String(expr), opts])
            .await
            .map_err(to_rpc_err)
    }

    async fn get_entity_count(&self) -> RpcResult<Value> {
        self.client
            .proxy("arkiv_getEntityCount", vec![])
            .await
            .map_err(to_rpc_err)
    }

    async fn get_block_timing(&self) -> RpcResult<BlockTimingResponse> {
        self.timing.block_timing().map_err(to_rpc_err)
    }
}

fn to_rpc_err(e: eyre::Report) -> ErrorObjectOwned {
    // -32000 = generic server error per JSON-RPC 2.0.
    ErrorObjectOwned::owned(-32000, e.to_string(), None::<()>)
}
