//! JSON-RPC storage backend for the Go EntityDB service.
//!
//! Implements `arkiv_commitChain`, `arkiv_revert`, and `arkiv_reorg`.

use alloy_primitives::B256;
use crate::storage::{ArkivBlock, ArkivBlockRef, Storage};
use eyre::{bail, Result};
use serde::Deserialize;
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
// JsonRpcStore
// ---------------------------------------------------------------------------

/// A [`Storage`] implementation that forwards blocks to a Go EntityDB via JSON-RPC.
pub struct JsonRpcStore {
    client: reqwest::Client,
    url: String,
    next_id: AtomicU64,
}

impl JsonRpcStore {
    pub fn new(url: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .pool_max_idle_per_host(1)
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
            url,
            next_id: AtomicU64::new(1),
        }
    }

    fn rpc_call<R: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<R> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": [params]
        });

        // Bridge async reqwest into sync Storage trait.
        // block_in_place is safe here — the ExEx runs on a multi-threaded runtime.
        let resp = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                self.client.post(&self.url).json(&body).send(),
            )
        })
        .map_err(|e| eyre::eyre!("EntityDB request failed: {}", e))?;

        let rpc_resp: JsonRpcResponse<R> = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(resp.json())
        })
        .map_err(|e| eyre::eyre!("EntityDB response parse failed: {}", e))?;

        match rpc_resp.error {
            Some(e) => bail!("EntityDB error {}: {}", e.code, e.message),
            None => rpc_resp
                .result
                .ok_or_else(|| eyre::eyre!("EntityDB returned empty result")),
        }
    }
}

impl Storage for JsonRpcStore {
    fn handle_commit(&self, blocks: &[ArkivBlock]) -> Result<Option<B256>> {
        let resp: CommitResponse = self.rpc_call(
            "arkiv_commitChain",
            serde_json::json!({ "blocks": blocks }),
        )?;
        Ok(Some(resp.state_root))
    }

    fn handle_revert(&self, blocks: &[ArkivBlockRef]) -> Result<Option<B256>> {
        let resp: CommitResponse = self.rpc_call(
            "arkiv_revert",
            serde_json::json!({ "blocks": blocks }),
        )?;
        Ok(Some(resp.state_root))
    }

    fn handle_reorg(
        &self,
        reverted: &[ArkivBlockRef],
        new_blocks: &[ArkivBlock],
    ) -> Result<Option<B256>> {
        let resp: CommitResponse = self.rpc_call(
            "arkiv_reorg",
            serde_json::json!({
                "revertedBlocks": reverted,
                "newBlocks": new_blocks
            }),
        )?;
        Ok(Some(resp.state_root))
    }
}
