// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

interface Vm {
    function envOr(string calldata name, address defaultValue) external view returns (address);
    function envOr(string calldata name, string calldata defaultValue) external view returns (string memory);
    function envOr(string calldata name, uint256 defaultValue) external view returns (uint256);
    function startBroadcast(uint256 privateKey) external;
    function stopBroadcast() external;
}

struct Mime128 {
    bytes32[4] data;
}

struct Attribute {
    bytes32 name;
    uint8 valueType;
    bytes32[4] value;
}

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
}

contract CreateSampleEntity {
    Vm private constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    address private constant DEFAULT_REGISTRY = 0x4200000000000000000000000000000000000042;
    uint256 private constant DEFAULT_PRIVATE_KEY = 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80;

    uint8 private constant OP_CREATE = 1;
    uint8 private constant ATTR_UINT = 1;
    uint8 private constant ATTR_STRING = 2;

    function run() external {
        uint256 privateKey = vm.envOr("PRIVATE_KEY", DEFAULT_PRIVATE_KEY);
        address registry = vm.envOr("REGISTRY_ADDRESS", DEFAULT_REGISTRY);
        uint256 expiresInBlocks = vm.envOr("EXPIRES_IN_BLOCKS", uint256(1_800));
        string memory payload = vm.envOr("PAYLOAD", string("arkiv forge sample entity"));

        Operation[] memory ops = new Operation[](1);
        ops[0] = Operation({
            operationType: OP_CREATE,
            entityKey: bytes32(0),
            payload: bytes(payload),
            contentType: mime128("text/plain"),
            attributes: sampleAttributes(),
            expiresAt: expiryBlock(expiresInBlocks),
            newOwner: address(0)
        });

        vm.startBroadcast(privateKey);
        IEntityRegistry(registry).execute(ops);
        vm.stopBroadcast();
    }

    function sampleAttributes() internal pure returns (Attribute[] memory attrs) {
        attrs = new Attribute[](3);
        // EntityRegistry requires attributes sorted lexicographically by packed name.
        attrs[0] = stringAttribute("category", "sample");
        attrs[1] = stringAttribute("source", "forge");
        attrs[2] = uintAttribute("version", 1);
    }

    function mime128(string memory value) internal pure returns (Mime128 memory) {
        return Mime128(bytes128Value(value));
    }

    function stringAttribute(string memory name, string memory value) internal pure returns (Attribute memory) {
        return Attribute({name: ident32(name), valueType: ATTR_STRING, value: bytes128Value(value)});
    }

    function uintAttribute(string memory name, uint256 value) internal pure returns (Attribute memory attr) {
        attr.name = ident32(name);
        attr.valueType = ATTR_UINT;
        attr.value[0] = bytes32(value);
    }

    function expiryBlock(uint256 expiresInBlocks) internal view returns (uint32 result) {
        require(block.number <= type(uint32).max, "block overflows uint32");
        require(expiresInBlocks <= type(uint32).max - block.number, "expiry overflows uint32");
        uint256 expiresAt = block.number + expiresInBlocks;
        assembly {
            result := expiresAt
        }
    }

    function ident32(string memory value) internal pure returns (bytes32 result) {
        bytes memory raw = bytes(value);
        require(raw.length > 0 && raw.length <= 32, "ident length must be 1-32 bytes");
        assembly {
            result := mload(add(raw, 32))
        }
    }

    function bytes128Value(string memory value) internal pure returns (bytes32[4] memory data) {
        bytes memory raw = bytes(value);
        require(raw.length <= 128, "value too long");
        for (uint256 i = 0; i < raw.length; i++) {
            data[i / 32] |= bytes32(uint256(uint8(raw[i])) << (248 - ((i % 32) * 8)));
        }
    }
}
