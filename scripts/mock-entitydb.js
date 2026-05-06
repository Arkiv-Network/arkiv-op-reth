#!/usr/bin/env node
// Mock EntityDB — pretty-prints incoming JSON-RPC request bodies and
// returns a dummy state root.
//
// Usage: node scripts/mock-entitydb.js [port]

const http = require("http");
const port = process.argv[2] || 9545;
const ZERO_ROOT =
  "0x0000000000000000000000000000000000000000000000000000000000000000";

const server = http.createServer((req, res) => {
  let body = "";
  req.on("data", (chunk) => (body += chunk));
  req.on("end", () => {
    let parsed;
    try {
      parsed = JSON.parse(body);
    } catch {
      res.writeHead(400);
      res.end("invalid json");
      return;
    }

    console.log(JSON.stringify(parsed, null, 2));

    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(
      JSON.stringify({
        jsonrpc: "2.0",
        id: parsed.id,
        result: dummyResultFor(parsed),
      })
    );
  });
});

// Dummy response shape per method. Write-side methods get a state-root envelope
// (matching what the ExEx parses); read-side methods get noop-shaped payloads
// matching the documented arkiv-op-reth API so the proxy round-trip is observable.
function dummyResultFor(req) {
  switch (req.method) {
    case "arkiv_query":
      return { data: [], blockNumber: "0x0" };
    case "arkiv_getEntityCount":
      return 0;
    case "arkiv_getBlockTiming":
      return { current_block: 0, current_block_time: 0, duration: 0 };
    case "arkiv_ping":
      return "pong";
    default:
      // arkiv_commitChain / arkiv_revert / arkiv_reorg and anything else.
      return { stateRoot: ZERO_ROOT };
  }
}

server.listen(port, () => {
  console.log(`mock-entitydb listening on http://localhost:${port}`);
});
