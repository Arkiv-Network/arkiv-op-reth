//! JSON-RPC storage backend for the Go EntityDB service.
//!
//! Implements `arkiv_commitChain`, `arkiv_revert`, and `arkiv_reorg`.
//! The underlying HTTP/JSON-RPC client is exposed as [`EntityDbClient`]
//! and shared with the `arkiv_query` RPC handler in `crate::rpc`.

use crate::storage::{ArkivBlock, ArkivBlockRef, Storage};
use alloy_primitives::B256;
use eyre::{Result, bail};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitResponse {
    state_root: B256,
}

// ---------------------------------------------------------------------------
// EntityDbClient — shared HTTP/JSON-RPC client
// ---------------------------------------------------------------------------

/// Thin JSON-RPC client for the EntityDB service.
///
/// The EntityDB exposes two ports: one for the ExEx write API
/// (`arkiv_commitChain`, `arkiv_revert`, `arkiv_reorg`) and one for the
/// query API (`arkiv_query`, `arkiv_getEntityCount`, `arkiv_getBlockTiming`).
/// `db_url` is used by the synchronous [`Self::rpc_call`] (write path) and
/// `query_url` by the async [`Self::proxy`] (read path). If no separate
/// query URL is provided, `db_url` is used for both.
pub struct EntityDbClient {
    http: reqwest::Client,
    db_url: String,
    query_url: String,
    next_id: AtomicU64,
}

impl EntityDbClient {
    pub fn new(db_url: String, query_url: Option<String>) -> Self {
        let query_url = query_url.unwrap_or_else(|| db_url.clone());
        Self {
            http: reqwest::Client::builder()
                .pool_max_idle_per_host(1)
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
            db_url,
            query_url,
            next_id: AtomicU64::new(1),
        }
    }

    /// Verify the EntityDB endpoints are reachable. Sends a trivial JSON-RPC
    /// request to each URL; transport errors are surfaced, RPC-level errors
    /// are ignored (the server is alive, which is all we want to confirm).
    pub async fn health_check(&self) -> Result<()> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "arkiv_ping",
            "params": []
        });
        self.http
            .post(&self.db_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre::eyre!("EntityDB unreachable at {}: {}", self.db_url, e))?;

        if self.query_url != self.db_url {
            self.http
                .post(&self.query_url)
                .json(&body)
                .send()
                .await
                .map_err(|e| eyre::eyre!("EntityDB query API unreachable at {}: {}", self.query_url, e))?;
        }
        Ok(())
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Sync RPC call — used by the ExEx, which already runs on a multi-threaded
    /// runtime and bridges async via `block_in_place`.
    pub fn rpc_call<R: DeserializeOwned>(&self, method: &str, params: Value) -> Result<R> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": method,
            "params": [params]
        });

        let resp = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.http.post(&self.db_url).json(&body).send())
        })
        .map_err(|e| eyre::eyre!("EntityDB request failed: {}", e))?;

        let rpc_resp: JsonRpcResponse<R> =
            tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(resp.json()))
                .map_err(|e| eyre::eyre!("EntityDB response parse failed: {}", e))?;

        unwrap_response(rpc_resp)
    }

    /// Async RPC proxy — used by the `arkiv_*` JSON-RPC handlers. The caller
    /// supplies the full positional-args array; it is forwarded verbatim and
    /// the raw `result` payload is returned. Note this differs from
    /// [`Self::rpc_call`], which wraps a single `Value` in a one-element
    /// array for the ExEx write-path methods.
    pub async fn proxy(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": method,
            "params": params,
        });

        let resp = self
            .http
            .post(&self.query_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre::eyre!("EntityDB query request failed: {}", e))?;

        let rpc_resp: JsonRpcResponse<Value> = resp
            .json()
            .await
            .map_err(|e| eyre::eyre!("EntityDB response parse failed: {}", e))?;

        unwrap_response(rpc_resp)
    }
}

fn unwrap_response<T: DeserializeOwned>(resp: JsonRpcResponse<T>) -> Result<T> {
    match resp.error {
        Some(e) => bail!("EntityDB error {}: {}", e.code, e.message),
        None => resp
            .result
            .ok_or_else(|| eyre::eyre!("EntityDB returned empty result")),
    }
}

// ---------------------------------------------------------------------------
// JsonRpcStore
// ---------------------------------------------------------------------------

/// A [`Storage`] implementation that forwards blocks to a Go EntityDB via JSON-RPC.
pub struct JsonRpcStore {
    client: Arc<EntityDbClient>,
}

impl JsonRpcStore {
    pub fn from_client(client: Arc<EntityDbClient>) -> Self {
        Self { client }
    }
}

impl Storage for JsonRpcStore {
    fn handle_commit(&self, blocks: &[ArkivBlock]) -> Result<Option<B256>> {
        let resp: CommitResponse = self
            .client
            .rpc_call("arkiv_commitChain", serde_json::json!({ "blocks": blocks }))?;
        Ok(Some(resp.state_root))
    }

    fn handle_revert(&self, blocks: &[ArkivBlockRef]) -> Result<Option<B256>> {
        let resp: CommitResponse = self
            .client
            .rpc_call("arkiv_revert", serde_json::json!({ "blocks": blocks }))?;
        Ok(Some(resp.state_root))
    }

    fn handle_reorg(
        &self,
        reverted: &[ArkivBlockRef],
        new_blocks: &[ArkivBlock],
    ) -> Result<Option<B256>> {
        let resp: CommitResponse = self.client.rpc_call(
            "arkiv_reorg",
            serde_json::json!({
                "revertedBlocks": reverted,
                "newBlocks": new_blocks
            }),
        )?;
        Ok(Some(resp.state_root))
    }
}
