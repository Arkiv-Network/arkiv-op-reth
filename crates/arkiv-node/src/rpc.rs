//! `arkiv_*` JSON-RPC namespace.
//!
//! Single endpoint `arkiv_query` that forwards its `params` verbatim to the
//! configured EntityDB and returns the raw `result`. The handler shares the
//! same [`EntityDbClient`] (and therefore the same connection pool) as the
//! ExEx's write-side `JsonRpcStore`.

use crate::storage::EntityDbClient;
use jsonrpsee::core::{RpcResult, async_trait};
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use serde_json::Value;
use std::sync::Arc;

#[rpc(server, namespace = "arkiv")]
pub trait ArkivApi {
    /// Forward an arbitrary query to the configured EntityDB. The `params`
    /// envelope is passed through unmodified.
    #[method(name = "query")]
    async fn query(&self, query: Value) -> RpcResult<Value>;
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
    async fn query(&self, query: Value) -> RpcResult<Value> {
        self.client
            .proxy("arkiv_query", query)
            .await
            .map_err(to_rpc_err)
    }
}

fn to_rpc_err(e: eyre::Report) -> ErrorObjectOwned {
    // -32000 = generic server error per JSON-RPC 2.0.
    ErrorObjectOwned::owned(-32000, e.to_string(), None::<()>)
}
