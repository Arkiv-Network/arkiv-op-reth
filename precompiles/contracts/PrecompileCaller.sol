// SPDX-License-Identifier: GPL-3.0-or-later
pragma solidity ^0.8.28;

/// @title  PrecompileCaller — POC harness for the Arkiv EntityDB-write precompile.
/// @notice Calls the precompile at 0x00…aa01 with arbitrary calldata, expects a
///         32-byte response (the EntityDB stateRoot), emits an event, and
///         returns the value so it can also be fetched via `cast call`.
contract PrecompileCaller {
    /// @notice Address of the Arkiv EntityDB-write precompile.
    /// @dev    Must match `ARKIV_PRECOMPILE_ADDRESS` in
    ///         `crates/arkiv-node/src/precompile.rs`.
    address public constant ARKIV_PRECOMPILE = 0x000000000000000000000000000000000000Aa01;

    /// @notice Last response returned by the precompile, for `cast call` retrieval.
    bytes32 public lastStateRoot;

    /// @notice Emitted on every successful precompile call.
    /// @param  caller     msg.sender of `callPrecompile`
    /// @param  data       Calldata forwarded to the precompile
    /// @param  stateRoot  32-byte response from the precompile (EntityDB's reply)
    event PrecompileResult(address indexed caller, bytes data, bytes32 stateRoot);

    /// @notice Forward `data` to the Arkiv precompile, store and emit the response.
    /// @param  data Arbitrary bytes; opaque to this contract, interpreted by EntityDB.
    /// @return stateRoot The 32-byte EntityDB stateRoot returned by the precompile.
    function callPrecompile(bytes calldata data) external returns (bytes32 stateRoot) {
        (bool ok, bytes memory ret) = ARKIV_PRECOMPILE.call(data);
        require(ok, "precompile call reverted");
        require(ret.length == 32, "precompile returned unexpected length");
        stateRoot = abi.decode(ret, (bytes32));
        lastStateRoot = stateRoot;
        emit PrecompileResult(msg.sender, data, stateRoot);
    }
}
