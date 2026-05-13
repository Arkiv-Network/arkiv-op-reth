// SPDX-License-Identifier: GPL-3.0-or-later
pragma solidity 0.8.25;

// ── Types ────────────────────────────────────────────────────────────────
//
// Inlined verbatim from arkiv-contracts v1 so the external ABI surface
// (function selectors, struct layouts, indexed event fields) matches the
// shape the Rust SDK (`arkiv-bindings`) was generated against. Validation
// helpers (`validateIdent32`, `validateMime128`, `encodeMime128`, …) are
// **not** inlined — content validation lives in the Arkiv precompile in
// v2.

/// @dev Block number encoded as uint32. Adopting v1's UDVT so v1-shaped
/// fields (`Operation.btl`, `EntityOperation.expiresAt`, …) stay ABI-
/// identical for SDK consumers. Only the operators the contract body
/// actually uses are kept — v1 declared 7 but this contract only needs
/// `<=`, `>`, and `+`.
type BlockNumber32 is uint32;

using {
    blockNumberLte as <=,
    blockNumberGt as >,
    blockNumberAdd as +
} for BlockNumber32 global;

function blockNumberLte(BlockNumber32 a, BlockNumber32 b) pure returns (bool) {
    return BlockNumber32.unwrap(a) <= BlockNumber32.unwrap(b);
}
function blockNumberGt(BlockNumber32 a, BlockNumber32 b) pure returns (bool) {
    return BlockNumber32.unwrap(a) > BlockNumber32.unwrap(b);
}
function blockNumberAdd(BlockNumber32 a, BlockNumber32 b) pure returns (BlockNumber32) {
    return BlockNumber32.wrap(BlockNumber32.unwrap(a) + BlockNumber32.unwrap(b));
}

/// @dev Validated lowercase-ASCII identifier (≤32 bytes, left-aligned).
/// v1 UDVT preserved so the SDK's `Ident32` wrapper resolves; in v2 the
/// charset validation runs inside the precompile, not on-chain.
type Ident32 is bytes32;

/// @dev 128-byte MIME type descriptor, four-word packed. v1 struct
/// preserved so the SDK's `Mime128` resolves. Validation runs in the
/// precompile.
struct Mime128 {
    bytes32[4] data;
}

// ── Entity library ───────────────────────────────────────────────────────
//
// Op-type constants, structs, and errors — names and signatures held
// identical to arkiv-contracts v1 so the SDK's generated bindings keep
// matching. The library has no logic; it's a pure type/constant
// container.

library Entity {
    uint8 internal constant UNINITIALIZED = 0;
    uint8 internal constant CREATE = 1;
    uint8 internal constant UPDATE = 2;
    uint8 internal constant EXTEND = 3;
    uint8 internal constant TRANSFER = 4;
    uint8 internal constant DELETE = 5;
    uint8 internal constant EXPIRE = 6;

    uint8 internal constant ATTR_UINT = 1;
    uint8 internal constant ATTR_STRING = 2;
    uint8 internal constant ATTR_ENTITY_KEY = 3;

    struct Attribute {
        Ident32 name;
        uint8 valueType;
        bytes32[4] value;
    }

    struct Operation {
        uint8 operationType;
        bytes32 entityKey;
        bytes payload;
        Mime128 contentType;
        Attribute[] attributes;
        BlockNumber32 btl;
        address newOwner;
    }

    // Errors — names + arg shapes preserved from v1.
    error EmptyBatch();
    error InvalidOpType(uint8 operationType);
    error ZeroBtl();
    error EntityNotFound(bytes32 entityKey);
    error NotOwner(bytes32 entityKey, address caller, address owner);
    error EntityExpired(bytes32 entityKey, BlockNumber32 expiresAt);
    error ExpiryNotExtended(
        bytes32 entityKey,
        BlockNumber32 newExpiresAt,
        BlockNumber32 currentExpiresAt
    );
    error TransferToZeroAddress(bytes32 entityKey);
    error TransferToSelf(bytes32 entityKey);
    error EntityNotExpired(bytes32 entityKey, BlockNumber32 expiresAt);
}

/// @title EntityRegistry
/// @notice User-facing entry point. Validates ownership + expiration per
///         op, mints entity keys, updates per-entity `(owner, expiresAt)`
///         storage, then dispatches the **validated batch** to the Arkiv
///         precompile in a single `CALL`.
///
///         The external ABI surface (`execute(Operation[])` selector,
///         `EntityOperation` event signature, `nonces(address)` and
///         `entityKey(address,uint32)` views) matches arkiv-contracts v1
///         so the existing SDK keeps working unchanged.
///
///         Internally, v2 stores only `(owner, expiresAt)` per entity —
///         everything else (payload, attributes, ID maps, bitmaps) lives
///         in precompile-managed accounts in the trie. The
///         `EntityOperation` event's `entityHash` field is always zero
///         in v2 (rolling-hash machinery was removed); the field is kept
///         in the signature so SDK decoders continue to deserialize.
contract EntityRegistry {
    /// @dev Adjacent to the registry (0x…0044) and the system account
    ///      (0x…0046). The custom EvmFactory inserts `ArkivPrecompile`
    ///      at this address.
    address public constant ARKIV_PRECOMPILE =
        0x4400000000000000000000000000000000000045;

    /// @dev v2 per-entity storage: only what the contract itself needs
    ///      to enforce ownership and expiration. Fits in one slot
    ///      (address 20B + BlockNumber32 4B).
    struct EntityRecord {
        address owner;
        BlockNumber32 expiresAt;
    }

    /// @dev Per-op record built by validation, dispatched to the
    ///      precompile in a single batched `CALL`. Internal to this
    ///      contract; not part of the SDK ABI.
    ///
    ///      The precompile reads the **old** `owner` and `expiresAt`
    ///      values from the existing entity account's RLP (which holds
    ///      them as well — intentional duplication so query reads
    ///      against the entity account are self-sufficient). The
    ///      contract therefore only forwards the **new** values for ops
    ///      that change them.
    ///
    ///      Per-op semantics:
    ///        CREATE:   newOwner = sender, newExpiresAt = current + btl,
    ///                  payload / contentType / attributes forwarded
    ///        UPDATE:   payload / contentType / attributes forwarded
    ///                  (precompile keeps owner / expiresAt from old RLP)
    ///        EXTEND:   newExpiresAt = current + btl
    ///                  (precompile keeps owner / payload / etc. from old RLP)
    ///        TRANSFER: newOwner = op.newOwner
    ///                  (precompile keeps expiresAt / payload / etc. from old RLP)
    ///        DELETE / EXPIRE: no new values — precompile reads old RLP
    ///                  for bitmap-removal targets and clears the account
    struct OpRecord {
        uint8 operationType;
        address sender;
        bytes32 entityKey;
        address newOwner;
        BlockNumber32 newExpiresAt;
        bytes payload;
        Mime128 contentType;
        Entity.Attribute[] attributes;
    }

    /// @notice Per-caller monotonic counter used to mint entity keys.
    /// Public + uint32 to match v1's `nonces(address) returns (uint32)`
    /// view that the SDK calls.
    mapping(address owner => uint32) public nonces;

    /// @notice Current owner + expiry for every live entity. Public so
    /// the auto-generated getter (`entities(bytes32) returns (address,
    /// BlockNumber32)`) gives the SDK a v2-equivalent of the v1
    /// `commitment(bytes32)` view. Phase 7 of the migration plan
    /// updates `arkiv-cli` to consume this shape.
    mapping(bytes32 entityKey => EntityRecord) public entities;

    /// @notice Emitted once per validated op. Signature held identical
    /// to v1 for SDK compatibility. The `entityHash` field is always
    /// `bytes32(0)` in v2 — the rolling EIP-712 hash machinery has been
    /// moved out of the contract.
    event EntityOperation(
        bytes32 indexed entityKey,
        uint8 indexed operationType,
        address indexed owner,
        BlockNumber32 expiresAt,
        bytes32 entityHash
    );

    error PrecompileFailed(bytes ret);

    /// @notice Submit a batch of operations atomically. Each op is
    ///         validated, applied to contract storage, and emits its
    ///         `EntityOperation` event in order. The resulting records
    ///         are then dispatched to the Arkiv precompile in a single
    ///         `CALL`. Any revert rolls back the whole batch.
    function execute(Entity.Operation[] calldata ops) external {
        if (ops.length == 0) revert Entity.EmptyBatch();

        BlockNumber32 current = BlockNumber32.wrap(uint32(block.number));
        OpRecord[] memory records = new OpRecord[](ops.length);

        for (uint256 i = 0; i < ops.length; ++i) {
            Entity.Operation calldata op = ops[i];
            uint8 t = op.operationType;
            if (t == Entity.CREATE) {
                records[i] = _create(op, current);
            } else if (t == Entity.UPDATE) {
                records[i] = _update(op, current);
            } else if (t == Entity.EXTEND) {
                records[i] = _extend(op, current);
            } else if (t == Entity.TRANSFER) {
                records[i] = _transfer(op, current);
            } else if (t == Entity.DELETE) {
                records[i] = _delete(op, current);
            } else if (t == Entity.EXPIRE) {
                records[i] = _expire(op, current);
            } else {
                revert Entity.InvalidOpType(t);
            }
        }

        _callPrecompile(records);
    }

    /// @notice Derive the entity key for a given owner and nonce.
    ///         v1-compatible: `keccak256(chainId || registry || owner || nonce)`.
    function entityKey(address owner, uint32 nonce) public view returns (bytes32) {
        return keccak256(abi.encodePacked(block.chainid, address(this), owner, nonce));
    }

    // ── Per-op handlers ─────────────────────────────────────────
    //
    // Each handler validates, updates contract storage, emits
    // `EntityOperation`, and returns its `OpRecord` for the batched
    // precompile call. `entityHash` in the emitted event is always 0
    // in v2 — see contract-level NatSpec.

    function _create(Entity.Operation calldata op, BlockNumber32 current)
        internal
        returns (OpRecord memory rec)
    {
        if (BlockNumber32.unwrap(op.btl) == 0) revert Entity.ZeroBtl();

        uint32 nonce = nonces[msg.sender]++;
        bytes32 key = entityKey(msg.sender, nonce);
        BlockNumber32 expiresAt = current + op.btl;
        entities[key] = EntityRecord({owner: msg.sender, expiresAt: expiresAt});

        emit EntityOperation(key, Entity.CREATE, msg.sender, expiresAt, bytes32(0));

        rec.operationType = Entity.CREATE;
        rec.sender = msg.sender;
        rec.entityKey = key;
        rec.newOwner = msg.sender;
        rec.newExpiresAt = expiresAt;
        rec.payload = op.payload;
        rec.contentType = op.contentType;
        rec.attributes = op.attributes;
    }

    function _update(Entity.Operation calldata op, BlockNumber32 current)
        internal
        returns (OpRecord memory rec)
    {
        EntityRecord storage stored = entities[op.entityKey];
        _requireExists(op.entityKey, stored);
        _requireActive(op.entityKey, stored, current);
        _requireOwner(op.entityKey, stored);

        emit EntityOperation(op.entityKey, Entity.UPDATE, stored.owner, stored.expiresAt, bytes32(0));

        rec.operationType = Entity.UPDATE;
        rec.sender = msg.sender;
        rec.entityKey = op.entityKey;
        rec.payload = op.payload;
        rec.contentType = op.contentType;
        rec.attributes = op.attributes;
    }

    function _extend(Entity.Operation calldata op, BlockNumber32 current)
        internal
        returns (OpRecord memory rec)
    {
        EntityRecord storage stored = entities[op.entityKey];
        _requireExists(op.entityKey, stored);
        _requireActive(op.entityKey, stored, current);
        _requireOwner(op.entityKey, stored);
        if (BlockNumber32.unwrap(op.btl) == 0) revert Entity.ZeroBtl();

        BlockNumber32 newExpiresAt = current + op.btl;
        if (newExpiresAt <= stored.expiresAt) {
            revert Entity.ExpiryNotExtended(op.entityKey, newExpiresAt, stored.expiresAt);
        }
        stored.expiresAt = newExpiresAt;

        emit EntityOperation(op.entityKey, Entity.EXTEND, stored.owner, newExpiresAt, bytes32(0));

        rec.operationType = Entity.EXTEND;
        rec.sender = msg.sender;
        rec.entityKey = op.entityKey;
        rec.newExpiresAt = newExpiresAt;
    }

    function _transfer(Entity.Operation calldata op, BlockNumber32 current)
        internal
        returns (OpRecord memory rec)
    {
        EntityRecord storage stored = entities[op.entityKey];
        _requireExists(op.entityKey, stored);
        _requireActive(op.entityKey, stored, current);
        _requireOwner(op.entityKey, stored);
        if (op.newOwner == address(0)) revert Entity.TransferToZeroAddress(op.entityKey);
        if (op.newOwner == stored.owner) revert Entity.TransferToSelf(op.entityKey);

        stored.owner = op.newOwner;

        emit EntityOperation(op.entityKey, Entity.TRANSFER, op.newOwner, stored.expiresAt, bytes32(0));

        rec.operationType = Entity.TRANSFER;
        rec.sender = msg.sender;
        rec.entityKey = op.entityKey;
        rec.newOwner = op.newOwner;
    }

    function _delete(Entity.Operation calldata op, BlockNumber32 current)
        internal
        returns (OpRecord memory rec)
    {
        EntityRecord storage stored = entities[op.entityKey];
        _requireExists(op.entityKey, stored);
        _requireActive(op.entityKey, stored, current);
        _requireOwner(op.entityKey, stored);

        // Snapshot for the event before clearing storage; the precompile
        // separately reads the entity RLP for bitmap-removal targets.
        address owner = stored.owner;
        BlockNumber32 expiresAt = stored.expiresAt;
        delete entities[op.entityKey];

        emit EntityOperation(op.entityKey, Entity.DELETE, owner, expiresAt, bytes32(0));

        rec.operationType = Entity.DELETE;
        rec.sender = msg.sender;
        rec.entityKey = op.entityKey;
    }

    function _expire(Entity.Operation calldata op, BlockNumber32 current)
        internal
        returns (OpRecord memory rec)
    {
        EntityRecord storage stored = entities[op.entityKey];
        _requireExists(op.entityKey, stored);
        if (stored.expiresAt > current) {
            revert Entity.EntityNotExpired(op.entityKey, stored.expiresAt);
        }

        address owner = stored.owner;
        BlockNumber32 expiresAt = stored.expiresAt;
        delete entities[op.entityKey];

        emit EntityOperation(op.entityKey, Entity.EXPIRE, owner, expiresAt, bytes32(0));

        rec.operationType = Entity.EXPIRE;
        rec.sender = msg.sender;
        rec.entityKey = op.entityKey;
    }

    // ── Guards ──────────────────────────────────────────────────

    function _requireExists(bytes32 key, EntityRecord storage stored) internal view {
        if (stored.owner == address(0)) revert Entity.EntityNotFound(key);
    }

    function _requireActive(bytes32 key, EntityRecord storage stored, BlockNumber32 current)
        internal
        view
    {
        if (stored.expiresAt <= current) revert Entity.EntityExpired(key, stored.expiresAt);
    }

    function _requireOwner(bytes32 key, EntityRecord storage stored) internal view {
        if (stored.owner != msg.sender) {
            revert Entity.NotOwner(key, msg.sender, stored.owner);
        }
    }

    // ── Precompile dispatch ─────────────────────────────────────

    function _callPrecompile(OpRecord[] memory records) internal {
        (bool success, bytes memory ret) =
            ARKIV_PRECOMPILE.call(abi.encode(records));
        if (!success) revert PrecompileFailed(ret);
    }
}
