//! `arkiv_*` JSON-RPC namespace.
//!
//! Registers a single method, `arkiv_query`, on the node's HTTP RPC
//! server. Implementation flow:
//!
//! 1. Parse the query string via [`arkiv_entitydb::query::parse`].
//! 2. Take a `StateProvider` snapshot at the head ([`StateProviderFactory::latest`]).
//! 3. Evaluate the query through a read-only [`RethStateAdapter`] →
//!    a [`Bitmap`] of entity IDs.
//! 4. Paginate descending (newest IDs first) and resolve each ID to an
//!    [`EntityData`] via [`arkiv_entitydb::resolve_id`].
//!
//! Phase 11 scope: head state only (no `atBlock`), no `includeData`
//! filtering, no separate `getEntityByKey` / `getEntityCount` methods.
//! All of those can land later without disturbing this shape.

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Bytes, U256};
use arkiv_entitydb::query::parse;
use arkiv_entitydb::{Bitmap, EntityRlp, StateAdapter, resolve_id};
use async_trait::async_trait;
use eyre::Result;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::error::{ErrorObject, ErrorObjectOwned, INTERNAL_ERROR_CODE};
use reth_storage_api::{StateProvider, StateProviderBox, StateProviderFactory};
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResponse {
    pub data: Vec<EntityData>,
    /// Block number at which the query was evaluated.
    pub block_number: u64,
    /// Cursor for the next page, or `None` if this is the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityData {
    pub key: B256,
    pub value: Bytes,
    pub content_type: String,
    pub expires_at: u64,
    pub owner: Address,
    pub creator: Address,
    pub created_at_block: u64,
    pub string_attributes: Vec<StringAttribute>,
    pub numeric_attributes: Vec<NumericAttribute>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StringAttribute {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NumericAttribute {
    pub key: String,
    pub value: U256,
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
    P: StateProviderFactory + Clone + Send + Sync + 'static,
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
}

// ── Query pipeline ───────────────────────────────────────────────────

fn run_query<P: StateProviderFactory>(
    provider: P,
    q: &str,
    options: &QueryOptions,
) -> Result<QueryResponse> {
    let parsed = parse(q)?;
    let (state, block_number) = snapshot_for(&provider, options.at_block)?;
    let mut adapter = RethStateAdapter::new(state);

    let bitmap = parsed.evaluate(&mut adapter)?;
    let cursor = parse_cursor(options.cursor.as_deref())?;

    let (data, next_cursor) = paginate(&mut adapter, bitmap, cursor, options.results_per_page)?;

    Ok(QueryResponse { data, block_number, cursor: next_cursor })
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

/// Descending pagination. Returns (page, next_cursor).
fn paginate(
    adapter: &mut RethStateAdapter,
    bitmap: Bitmap,
    cursor: Option<u64>,
    page_size_req: Option<u64>,
) -> Result<(Vec<EntityData>, Option<String>)> {
    let page_size = page_size_req
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE) as usize;

    // Collect ascending, drop IDs >= cursor, iterate in reverse for
    // newest-first. Page sizes are small (≤200) so the materialized
    // Vec is fine even when the bitmap is large.
    let mut ids: Vec<u64> = bitmap.iter().collect();
    ids.sort_unstable();
    if let Some(c) = cursor {
        while ids.last().is_some_and(|id| *id >= c) {
            ids.pop();
        }
    }

    let mut data = Vec::with_capacity(page_size.min(ids.len()));
    let mut last_returned_id: Option<u64> = None;
    let mut has_more = false;

    for &id in ids.iter().rev() {
        if data.len() >= page_size {
            has_more = true;
            break;
        }
        // Entities can race delete between the bitmap read and the
        // resolve — `None` means the entity was tombstoned; skip.
        if let Some(entity) = resolve_id(adapter, id)? {
            data.push(entity_data_from(entity));
            last_returned_id = Some(id);
        }
    }

    let next_cursor = if has_more {
        last_returned_id.map(|id| format!("0x{id:x}"))
    } else {
        None
    };
    Ok((data, next_cursor))
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
