# arkiv-reth
Arkiv reth execution client

## Storage backends

The `arkiv-node` ExEx persists `EntityRegistry` mutations into a configurable
storage backend. The backend is chosen from environment variables at startup:

| Variable              | Backend                                                            |
| --------------------- | ------------------------------------------------------------------ |
| `ARKIV_ROCKSDB_PATH`  | Embedded RocksDB store at the given path (recommended for prod).   |
| `ARKIV_ENTITYDB_URL`  | External Go EntityDB JSON-RPC backend.                             |
| _(unset)_             | Tracing/logging backend (development only — does not persist).     |

When `ARKIV_ROCKSDB_PATH` is used, setting `ARKIV_RPC_BIND` (e.g.
`127.0.0.1:8546`) also starts a JSON-RPC server exposing the
[`@arkiv-network/sdk`](https://github.com/Arkiv-Network/arkiv-sdk-js)
query API:

* `arkiv_query(query, options)`
* `arkiv_getEntity(key)`
* `arkiv_getEntityCount()`
* `arkiv_getBlockTiming()`

`query` is a string in the SDK's predicate grammar (e.g.
`name = "John" && (age > 5 || $owner = 0xabc...)`).
`$key`, `$owner` and `$creator` are metadata selectors.
