use crate::storage::rocks::{QueryOptions, QueryResponse, RocksDbStore};
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use std::sync::Arc;

#[rpc(server, namespace = "arkiv")]
pub trait ArkivApi {
    #[method(name = "query")]
    fn query(&self, query: String, options: Option<QueryOptions>) -> RpcResult<QueryResponse>;

    #[method(name = "getEntityCount")]
    fn entity_count(&self) -> RpcResult<usize>;
}

pub struct ArkivRpc {
    store: Arc<RocksDbStore>,
}

impl ArkivRpc {
    pub fn new(store: Arc<RocksDbStore>) -> Self {
        Self { store }
    }
}

impl ArkivApiServer for ArkivRpc {
    fn query(&self, query: String, options: Option<QueryOptions>) -> RpcResult<QueryResponse> {
        self.store
            .query(&query, options.unwrap_or_default())
            .map_err(rpc_error)
    }

    fn entity_count(&self) -> RpcResult<usize> {
        self.store.entity_count().map_err(rpc_error)
    }
}

fn rpc_error(error: eyre::Report) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(-32000, error.to_string(), None::<()>)
}
