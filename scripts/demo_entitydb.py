#!/usr/bin/env python3
import copy
import hashlib
import json
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class DemoState:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.entities = {}
        self.head_block = 0
        self.snapshots = {0: {"state": {}, "previous": 0}}

    @staticmethod
    def _block_number(value) -> int:
        if isinstance(value, int):
            return value
        return int(value, 16)

    def _state_root(self) -> str:
        payload = json.dumps(self.entities, sort_keys=True, separators=(",", ":")).encode()
        return "0x" + hashlib.sha256(payload).hexdigest()

    def commit(self, blocks):
        with self.lock:
            for block in blocks:
                block_number = self._block_number(block["header"]["number"])
                previous = self.head_block
                for tx in block.get("transactions", []):
                    for op in tx.get("operations", []):
                        self._apply_operation(op)
                self.head_block = block_number
                self.snapshots[block_number] = {
                    "state": copy.deepcopy(self.entities),
                    "previous": previous,
                }
            return {"stateRoot": self._state_root()}

    def revert(self, blocks):
        with self.lock:
            for block in blocks:
                block_number = self._block_number(block["number"])
                snapshot = self.snapshots.get(block_number)
                previous = snapshot["previous"] if snapshot else 0
                prior = self.snapshots.get(previous, {"state": {}, "previous": 0})
                self.entities = copy.deepcopy(prior["state"])
                self.head_block = previous
            return {"stateRoot": self._state_root()}

    def reorg(self, reverted, new_blocks):
        with self.lock:
            for block in reverted:
                block_number = self._block_number(block["number"])
                snapshot = self.snapshots.get(block_number)
                previous = snapshot["previous"] if snapshot else 0
                prior = self.snapshots.get(previous, {"state": {}, "previous": 0})
                self.entities = copy.deepcopy(prior["state"])
                self.head_block = previous
            for block in new_blocks:
                block_number = self._block_number(block["header"]["number"])
                previous = self.head_block
                for tx in block.get("transactions", []):
                    for op in tx.get("operations", []):
                        self._apply_operation(op)
                self.head_block = block_number
                self.snapshots[block_number] = {
                    "state": copy.deepcopy(self.entities),
                    "previous": previous,
                }
            return {"stateRoot": self._state_root()}

    def query(self, filter_expr):
        with self.lock:
            items = list(self.entities.values())
            if filter_expr != "*":
                if match := re.fullmatch(r'\$contentType = "([^"]+)"', filter_expr):
                    want = match.group(1)
                    items = [item for item in items if item["contentType"] == want]
                elif match := re.fullmatch(r"\$owner = (0x[0-9a-fA-F]+)", filter_expr):
                    want = match.group(1).lower()
                    items = [item for item in items if item["owner"].lower() == want]
                elif match := re.fullmatch(r"\$expiration < (\d+)", filter_expr):
                    limit = int(match.group(1))
                    items = [item for item in items if item["expiresAt"] < limit]
                else:
                    raise ValueError(f"unsupported filter: {filter_expr}")

            items.sort(key=lambda item: item["key"])
            return {
                "blockNumber": self.head_block,
                "data": items,
            }

    def _apply_operation(self, op):
        op_type = op["type"]
        key = op["entityKey"]
        owner = op["owner"]

        if op_type == "create":
            self.entities[key] = {
                "key": key,
                "contentType": op["contentType"],
                "expiresAt": self._block_number(op["expiresAt"]),
                "owner": owner,
            }
        elif op_type == "update":
            entity = self.entities.setdefault(
                key,
                {"key": key, "contentType": "", "expiresAt": 0, "owner": owner},
            )
            entity["contentType"] = op["contentType"]
            entity["owner"] = owner
        elif op_type == "extend":
            if key in self.entities:
                self.entities[key]["expiresAt"] = self._block_number(op["expiresAt"])
                self.entities[key]["owner"] = owner
        elif op_type == "transfer":
            if key in self.entities:
                self.entities[key]["owner"] = owner
        elif op_type in {"delete", "expire"}:
            self.entities.pop(key, None)
        else:
            raise ValueError(f"unsupported operation type: {op_type}")


STATE = DemoState()


class DemoHandler(BaseHTTPRequestHandler):
    server_version = "DemoEntityDB/1.0"

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        request = json.loads(self.rfile.read(length))
        method = request.get("method")
        params = request.get("params", [])

        try:
            if method == "arkiv_ping":
                result = True
            elif method == "arkiv_commitChain":
                result = STATE.commit(params[0].get("blocks", []))
            elif method == "arkiv_revert":
                result = STATE.revert(params[0].get("blocks", []))
            elif method == "arkiv_reorg":
                payload = params[0]
                result = STATE.reorg(
                    payload.get("revertedBlocks", []),
                    payload.get("newBlocks", []),
                )
            elif method == "arkiv_query":
                result = STATE.query(params[0])
            else:
                raise ValueError(f"unsupported method: {method}")
            response = {"jsonrpc": "2.0", "id": request.get("id"), "result": result}
        except Exception as exc:  # noqa: BLE001
            response = {
                "jsonrpc": "2.0",
                "id": request.get("id"),
                "error": {"code": -32000, "message": str(exc)},
            }

        body = json.dumps(response).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):  # noqa: A003
        return


def serve(port: int):
    server = ThreadingHTTPServer(("127.0.0.1", port), DemoHandler)
    server.serve_forever()


if __name__ == "__main__":
    threads = [
        threading.Thread(target=serve, args=(2704,), daemon=True),
        threading.Thread(target=serve, args=(2705,), daemon=True),
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
