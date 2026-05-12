//! `arkiv_*` JSON-RPC namespace.
//!
//! Transparent proxy: each method forwards its positional args verbatim to
//! the configured EntityDB and returns the raw `result`. The handler shares
//! the same [`EntityDbClient`] (and connection pool) as the ExEx's write-side
//! `JsonRpcStore`.

use crate::storage::EntityDbClient;
use jsonrpsee::core::{RpcResult, async_trait};
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use serde_json::Value;
use std::sync::Arc;

#[rpc(server, namespace = "arkiv")]
pub trait ArkivApi {
    /// Query entities. `expr` is the EntityDB query expression; `options`
    /// is an optional object (paging / projection / atBlock).
    #[method(name = "query")]
    async fn query(&self, expr: String, options: Option<Value>) -> RpcResult<Value>;

    /// Total entity count currently stored in EntityDB.
    #[method(name = "getEntityCount")]
    async fn get_entity_count(&self) -> RpcResult<Value>;

    /// Timing for the current head block.
    #[method(name = "getBlockTiming")]
    async fn get_block_timing(&self) -> RpcResult<Value>;
}

pub struct ArkivRpc {
    client: Arc<EntityDbClient>,
}

impl ArkivRpc {
    pub fn new(client: Arc<EntityDbClient>) -> Self {
        Self { client }
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

    async fn get_block_timing(&self) -> RpcResult<Value> {
        self.client
            .proxy("arkiv_getBlockTiming", vec![])
            .await
            .map_err(to_rpc_err)
    }
}

fn to_rpc_err(e: eyre::Report) -> ErrorObjectOwned {
    // -32000 = generic server error per JSON-RPC 2.0.
    ErrorObjectOwned::owned(-32000, e.to_string(), None::<()>)
}
