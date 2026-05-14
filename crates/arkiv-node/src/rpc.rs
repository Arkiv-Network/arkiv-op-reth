//! `arkiv_*` JSON-RPC namespace.
//!
//! Registers a single method, `arkiv_query`, on the node's HTTP RPC
//! server. This module is intentionally thin: it owns the JSON-RPC
//! plumbing, the wire-format types, and the `StateProvider` snapshot
//! selection — and delegates everything else to
//! [`arkiv_entitydb::query::execute`].
//!
//! Per-request flow:
//!
//! 1. Resolve `at_block` → a [`StateProvider`] snapshot + the actual
//!    block number to report back.
//! 2. Wrap the snapshot in a read-only [`RethStateAdapter`] and call
//!    [`execute`].
//! 3. Render the returned [`EntityRlp`]s into wire-format
//!    [`EntityData`] and encode the next cursor as a hex string.
//!
//! Phase 11 scope: no `includeData` field selection, no separate
//! `getEntityByKey` / `getEntityCount` methods. Both can land later
//! without changing the call shape.

use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Bytes, U256};
use arkiv_entitydb::query::{Page, PageParams, execute};
use arkiv_entitydb::{EntityRlp, StateAdapter, all_entities};
use async_trait::async_trait;
use eyre::Result;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::error::{ErrorObject, ErrorObjectOwned, INTERNAL_ERROR_CODE};
use reth_storage_api::{HeaderProvider, StateProvider, StateProviderBox, StateProviderFactory};
use serde::{Deserialize, Serialize};

/// Default `resultsPerPage` if not specified by the caller.
const DEFAULT_PAGE_SIZE: u64 = 100;
/// Hard cap on `resultsPerPage` — matches arkiv-storage-service.
const MAX_PAGE_SIZE: u64 = 200;

#[rpc(server, namespace = "arkiv")]
pub trait ArkivApi {
    /// Evaluate a query and return matching entities. Pagination is
    /// descending by entity ID (newest first). When more results
    /// remain, `cursor` in the response is the ID of the last entry —
    /// pass it back as `options.cursor` to fetch the next page.
    #[method(name = "query")]
    async fn query(
        &self,
        q: String,
        options: Option<QueryOptions>,
    ) -> RpcResult<QueryResponse>;

    /// Number of live entities at the head (`$all` bitmap cardinality).
    #[method(name = "getEntityCount")]
    async fn get_entity_count(&self) -> RpcResult<u64>;

    /// Head block number, head block timestamp, and the duration
    /// (seconds) between the head and its parent. Field names match
    /// op-geth's `arkivAPI.GetBlockTiming` JSON wire shape.
    #[method(name = "getBlockTiming")]
    async fn get_block_timing(&self) -> RpcResult<BlockTiming>;
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryOptions {
    /// Block to evaluate against. `None` or `"latest"` reads head; a
    /// hex number (`"0x1a"`) reads historical state. The SDK's
    /// `hexutil.Uint64` shape (just a hex string) is compatible —
    /// `BlockNumberOrTag` is a superset that also accepts the
    /// standard JSON-RPC tags (`earliest`, `pending`, `finalized`,
    /// `safe`).
    pub at_block: Option<BlockNumberOrTag>,
    /// Page size; clamped to `[1, 200]`. Defaults to 100.
    pub results_per_page: Option<u64>,
    /// Hex-encoded entity ID. Next page contains IDs strictly less
    /// than this value.
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResponse {
    pub data: Vec<EntityData>,
    /// Block number at which the query was evaluated.
    pub block_number: u64,
    /// Cursor for the next page, or `None` if this is the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityData {
    pub key: B256,
    pub value: Bytes,
    pub content_type: String,
    pub expires_at: u64,
    pub owner: Address,
    pub creator: Address,
    pub created_at_block: u64,
    /// Block of the most recent mutation (CREATE / UPDATE / EXTEND /
    /// TRANSFER). Equal to `created_at_block` until the entity is
    /// first modified.
    pub last_modified_at_block: u64,
    /// Tx-position metadata. Reth's revm context doesn't expose the
    /// tx-index-in-block during precompile execution, so we report 0
    /// here today — included for SDK wire-shape parity. Same applies
    /// to `operation_index_in_transaction`. Real values require a
    /// non-trivial block-builder-side annotation that we haven't
    /// landed yet.
    pub transaction_index_in_block: u64,
    pub operation_index_in_transaction: u64,
    pub string_attributes: Vec<StringAttribute>,
    pub numeric_attributes: Vec<NumericAttribute>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StringAttribute {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NumericAttribute {
    pub key: String,
    pub value: U256,
}

/// Response shape for `arkiv_getBlockTiming`. Snake_case on the wire —
/// the SDK reads `current_block` / `current_block_time` / `duration`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BlockTiming {
    /// Head block number.
    pub current_block: u64,
    /// Head block timestamp (seconds since epoch).
    pub current_block_time: u64,
    /// Seconds between the head and its parent.
    pub duration: u64,
}

/// JSON-RPC handler for the `arkiv_*` namespace.
pub struct ArkivRpc<P> {
    provider: P,
}

impl<P> ArkivRpc<P> {
    pub fn new(provider: P) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl<P> ArkivApiServer for ArkivRpc<P>
where
    P: StateProviderFactory + HeaderProvider + Clone + Send + Sync + 'static,
{
    async fn query(
        &self,
        q: String,
        options: Option<QueryOptions>,
    ) -> RpcResult<QueryResponse> {
        let provider = self.provider.clone();
        let options = options.unwrap_or_default();
        // State reads against MDBX are sync I/O — offload to a blocking
        // worker so we don't tie up the tokio runtime.
        tokio::task::spawn_blocking(move || run_query(provider, &q, &options))
            .await
            .map_err(|e| internal_err(format!("blocking task join: {e}")))?
            .map_err(|e| internal_err(format!("{e}")))
    }

    async fn get_entity_count(&self) -> RpcResult<u64> {
        let provider = self.provider.clone();
        tokio::task::spawn_blocking(move || -> Result<u64> {
            let mut adapter = RethStateAdapter::new(provider.latest()?);
            Ok(all_entities(&mut adapter)?.len())
        })
        .await
        .map_err(|e| internal_err(format!("blocking task join: {e}")))?
        .map_err(|e| internal_err(format!("{e}")))
    }

    async fn get_block_timing(&self) -> RpcResult<BlockTiming> {
        let provider = self.provider.clone();
        tokio::task::spawn_blocking(move || -> Result<BlockTiming> {
            let current_block = provider.best_block_number()?;
            let head = provider
                .header_by_number(current_block)?
                .ok_or_else(|| eyre::eyre!("head header missing for block {current_block}"))?;
            let current_block_time = head.timestamp();
            // Genesis has no parent — report duration=0 in that case.
            let duration = if current_block == 0 {
                0
            } else {
                let parent = provider
                    .header_by_number(current_block - 1)?
                    .ok_or_else(|| {
                        eyre::eyre!("parent header missing for block {}", current_block - 1)
                    })?;
                current_block_time.saturating_sub(parent.timestamp())
            };
            Ok(BlockTiming { current_block, current_block_time, duration })
        })
        .await
        .map_err(|e| internal_err(format!("blocking task join: {e}")))?
        .map_err(|e| internal_err(format!("{e}")))
    }
}

// ── Query pipeline ───────────────────────────────────────────────────

fn run_query<P: StateProviderFactory>(
    provider: P,
    q: &str,
    options: &QueryOptions,
) -> Result<QueryResponse> {
    let (state, block_number) = snapshot_for(&provider, options.at_block)?;
    let mut adapter = RethStateAdapter::new(state);

    let params = PageParams {
        page_size: options
            .results_per_page
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE),
        cursor: parse_cursor(options.cursor.as_deref())?,
    };
    let Page { entries, next_cursor } = execute(&mut adapter, q, params)?;

    Ok(QueryResponse {
        data: entries.into_iter().map(entity_data_from).collect(),
        block_number,
        cursor: next_cursor.map(|id| format!("0x{id:x}")),
    })
}

/// Resolve `at_block` to a concrete `(StateProvider, block_number)`.
/// `None` / `Latest` reads head; an explicit number reads historical
/// canonical state. Other tags are rejected for v1.
fn snapshot_for<P: StateProviderFactory>(
    provider: &P,
    at_block: Option<BlockNumberOrTag>,
) -> Result<(StateProviderBox, u64)> {
    match at_block {
        None | Some(BlockNumberOrTag::Latest) => {
            // Small race between best_block_number() and latest() is
            // acceptable — both observe the canonical head.
            let n = provider.best_block_number()?;
            Ok((provider.latest()?, n))
        }
        Some(BlockNumberOrTag::Number(n)) => {
            let state = provider.history_by_block_number(n)?;
            Ok((state, n))
        }
        Some(other) => eyre::bail!(
            "atBlock tag {other:?} not supported; pass a hex block number or 'latest'"
        ),
    }
}

fn entity_data_from(e: EntityRlp) -> EntityData {
    EntityData {
        key: e.key,
        value: Bytes::from(e.payload),
        content_type: String::from_utf8_lossy(&e.content_type).into_owned(),
        expires_at: e.expires_at,
        owner: e.owner,
        creator: e.creator,
        created_at_block: e.created_at_block,
        last_modified_at_block: e.last_modified_at_block,
        // Not yet tracked through the precompile path — see field doc.
        transaction_index_in_block: 0,
        operation_index_in_transaction: 0,
        string_attributes: e
            .string_annotations
            .into_iter()
            .map(|sa| StringAttribute {
                key: String::from_utf8_lossy(&sa.key).into_owned(),
                value: String::from_utf8_lossy(&sa.value).into_owned(),
            })
            .collect(),
        numeric_attributes: e
            .numeric_annotations
            .into_iter()
            .map(|na| NumericAttribute {
                key: String::from_utf8_lossy(&na.key).into_owned(),
                value: na.value,
            })
            .collect(),
    }
}

fn parse_cursor(s: Option<&str>) -> Result<Option<u64>> {
    match s {
        None => Ok(None),
        Some(c) => {
            let stripped = c.strip_prefix("0x").unwrap_or(c);
            let n = u64::from_str_radix(stripped, 16)
                .map_err(|e| eyre::eyre!("invalid cursor {c:?}: {e}"))?;
            Ok(Some(n))
        }
    }
}

fn internal_err(msg: String) -> ErrorObjectOwned {
    ErrorObject::owned(INTERNAL_ERROR_CODE, msg, None::<()>)
}

// ── Read-only StateAdapter over reth's StateProvider ─────────────────

/// `StateAdapter` impl backed by a reth [`StateProvider`] snapshot.
/// Used by the query RPC to drive [`arkiv_entitydb`]'s evaluator over
/// committed state. Mutating methods bail — they shouldn't be reached
/// from the read-only query path.
pub struct RethStateAdapter {
    state: StateProviderBox,
}

impl RethStateAdapter {
    pub fn new(state: StateProviderBox) -> Self {
        Self { state }
    }
}

impl StateAdapter for RethStateAdapter {
    fn code(&mut self, addr: &Address) -> Result<Vec<u8>> {
        Ok(self
            .state
            .account_code(addr)
            .map_err(|e| eyre::eyre!("account_code({addr}): {e}"))?
            .map(|bc| bc.original_bytes().to_vec())
            .unwrap_or_default())
    }

    fn storage(&mut self, addr: &Address, slot: B256) -> Result<B256> {
        let v = self
            .state
            .storage(*addr, slot)
            .map_err(|e| eyre::eyre!("storage({addr}, {slot}): {e}"))?
            .unwrap_or(U256::ZERO);
        Ok(B256::from(v.to_be_bytes()))
    }

    fn set_code(&mut self, _addr: &Address, _code: Vec<u8>) -> Result<()> {
        eyre::bail!("RethStateAdapter is read-only: set_code called from query path")
    }

    fn tombstone_code(&mut self, _addr: &Address) -> Result<()> {
        eyre::bail!("RethStateAdapter is read-only: tombstone_code called from query path")
    }

    fn set_storage(&mut self, _addr: &Address, _slot: B256, _value: B256) -> Result<()> {
        eyre::bail!("RethStateAdapter is read-only: set_storage called from query path")
    }
}
