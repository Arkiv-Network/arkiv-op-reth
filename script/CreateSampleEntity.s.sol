// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title CreateSampleEntity
/// @notice Forge script that creates a sample entity with several typed
/// attributes on the EntityRegistry predeploy.
///
/// The EntityRegistry contract is auto-deployed by `arkiv-node` at the
/// predeploy address `0x4200000000000000000000000000000000000042` via the
/// genesis allocation in `crates/arkiv-node/src/genesis.rs`. This script
/// targets that address — no deployment step is required.
///
/// The struct layouts and constants below mirror `Entity.sol` from
/// arkiv-contracts (rev `344640e`), which is the same source the Rust
/// bindings (`arkiv-bindings`) are generated from. They are inlined here
/// so the script has zero external Solidity dependencies (no submodules,
/// no `forge install` step required).
///
/// Usage (against `just node-dev`):
///
///     forge script script/CreateSampleEntity.s.sol \
///         --rpc-url http://localhost:8545 \
///         --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
///         --broadcast \
///         --legacy --gas-price 1000000000
///
/// or via the justfile recipe:
///
///     just sample-entity

// ─── Minimal forge-std-free cheatcode interface ──────────────────────────────

interface Vm {
    function startBroadcast() external;
    function startBroadcast(uint256 privateKey) external;
    function stopBroadcast() external;
    function envOr(string calldata name, address defaultValue) external view returns (address);
    function envOr(string calldata name, uint256 defaultValue) external view returns (uint256);
}

// ─── Structs mirrored from contracts/Entity.sol ──────────────────────────────

/// @dev Mirrors `Mime128` from `contracts/types/Mime128.sol`.
struct Mime128 {
    bytes32[4] data;
}

/// @dev Mirrors `Entity.Attribute`. `name` is the unwrapped `Ident32`
/// (left-aligned, zero-padded UTF-8). `valueType` is one of ATTR_UINT (1),
/// ATTR_STRING (2), or ATTR_ENTITY_KEY (3). `value` is a fixed 128-byte
/// container; encoding depends on `valueType` and is enforced off-chain.
struct Attribute {
    bytes32 name;
    uint8 valueType;
    bytes32[4] value;
}

/// @dev Mirrors `Entity.Operation`. `expiresAt` is the absolute block number
/// at which the entity expires. For CREATE, `entityKey` is ignored — the
/// registry derives it from (chainId, registry, owner, nonce).
struct Operation {
    uint8 operationType;
    bytes32 entityKey;
    bytes payload;
    Mime128 contentType;
    Attribute[] attributes;
    uint32 expiresAt;
    address newOwner;
}

interface IEntityRegistry {
    function execute(Operation[] calldata ops) external;
    function nonces(address owner) external view returns (uint32);
    function entityKey(address owner, uint32 nonce) external view returns (bytes32);
}

// ─── Script ──────────────────────────────────────────────────────────────────

contract CreateSampleEntity {
    // forge-std cheatcode address (HEVM_ADDRESS).
    Vm constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    // EntityRegistry predeploy, set up by `arkiv-node` genesis.
    address constant DEFAULT_REGISTRY = 0x4200000000000000000000000000000000000042;

    // Operation type discriminator for CREATE (see Entity.sol).
    uint8 constant OP_CREATE = 1;

    // Attribute valueType discriminators (see Entity.sol). The contract
    // validates `valueType >= ATTR_UINT && valueType <= ATTR_ENTITY_KEY`.
    uint8 constant ATTR_UINT = 1;
    uint8 constant ATTR_STRING = 2;
    uint8 constant ATTR_ENTITY_KEY = 3;

    /// @notice Default entrypoint. Builds a CREATE operation with three
    /// attributes (uint, string, entity-key) and submits it.
    /// Honors the `REGISTRY` and `EXPIRES_IN_BLOCKS` env vars when set.
    function run() external {
        address registry = vm.envOr("REGISTRY", DEFAULT_REGISTRY);
        // ~1 hour at 2s blocks ≈ 1800 blocks.
        uint256 expiresInBlocks = vm.envOr("EXPIRES_IN_BLOCKS", uint256(1800));
        // truncation to uint32 mirrors the Rust CLI; safe for any realistic
        // dev-chain block height.
        // forge-lint: disable-next-line(unsafe-typecast)
        uint32 expiresAt = uint32(block.number + expiresInBlocks);

        // Build attributes. They MUST be sorted strictly ascending by the
        // packed `name` (interpreted as big-endian uint256). Lexicographic
        // order on left-aligned ASCII names is sufficient: "author" <
        // "count" < "tag".
        Attribute[] memory attrs = new Attribute[](3);
        attrs[0] = stringAttr("author", "arkiv-sample");
        attrs[1] = uintAttr("count", 42);
        attrs[2] = entityKeyAttr("tag", keccak256("sample-tag"));

        Operation[] memory ops = new Operation[](1);
        ops[0] = Operation({
            operationType: OP_CREATE,
            entityKey: bytes32(0), // ignored for CREATE
            payload: bytes("hello, arkiv"),
            contentType: encodeMime("text/plain"),
            attributes: attrs,
            expiresAt: expiresAt,
            newOwner: address(0)
        });

        vm.startBroadcast();
        IEntityRegistry(registry).execute(ops);
        vm.stopBroadcast();
    }

    // ─── Encoding helpers ────────────────────────────────────────────────────

    /// @dev Pack a string into a left-aligned, zero-padded `bytes32`
    /// (the unwrapped `Ident32` representation). Reverts if longer than
    /// 32 bytes. Names must satisfy the Ident32 charset (a-z, 0-9, '.',
    /// '-', '_'; leading char a-z) — validated on-chain.
    function packIdent32(string memory name) internal pure returns (bytes32 out) {
        bytes memory b = bytes(name);
        require(b.length > 0 && b.length <= 32, "ident32: bad length");
        assembly {
            out := mload(add(b, 32))
        }
    }

    /// @dev Pack a string into a `Mime128` (left-aligned, zero-padded across
    /// 4 x bytes32 = 128 bytes). Reverts if longer than 128 bytes. The MIME
    /// structure (`type/subtype[; param=value]*`) is validated on-chain.
    function encodeMime(string memory value) internal pure returns (Mime128 memory m) {
        bytes memory b = bytes(value);
        require(b.length > 0 && b.length <= 128, "mime128: bad length");
        for (uint256 i = 0; i < b.length; i++) {
            uint256 slot = i / 32;
            uint256 offset = i % 32;
            m.data[slot] |= bytes32(bytes1(b[i])) >> (offset * 8);
        }
    }

    /// @dev Build an ATTR_UINT attribute. Value is a 32-byte big-endian
    /// uint256 in slot 0; remaining slots are zero.
    function uintAttr(string memory name, uint256 value) internal pure returns (Attribute memory) {
        bytes32[4] memory v;
        v[0] = bytes32(value);
        return Attribute({name: packIdent32(name), valueType: ATTR_UINT, value: v});
    }

    /// @dev Build an ATTR_STRING attribute. Value is raw UTF-8 bytes packed
    /// across the 128-byte container, left-aligned and zero-padded.
    function stringAttr(string memory name, string memory value) internal pure returns (Attribute memory) {
        bytes memory b = bytes(value);
        require(b.length <= 128, "string attr: too long");
        bytes32[4] memory v;
        for (uint256 i = 0; i < b.length; i++) {
            uint256 slot = i / 32;
            uint256 offset = i % 32;
            v[slot] |= bytes32(bytes1(b[i])) >> (offset * 8);
        }
        return Attribute({name: packIdent32(name), valueType: ATTR_STRING, value: v});
    }

    /// @dev Build an ATTR_ENTITY_KEY attribute. Value is a raw bytes32 in
    /// slot 0; remaining slots are zero.
    function entityKeyAttr(string memory name, bytes32 value) internal pure returns (Attribute memory) {
        bytes32[4] memory v;
        v[0] = value;
        return Attribute({name: packIdent32(name), valueType: ATTR_ENTITY_KEY, value: v});
    }
}
