#!/usr/bin/env node
// Mock EntityDB — logs incoming JSON-RPC requests and returns a dummy state root.
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

    const method = parsed.method;
    const params = parsed.params?.[0];

    console.log(`\n=== ${method} ===`);
    console.log(JSON.stringify(params, null, 2));

    // Count operations across blocks
    if (params?.blocks) {
      let ops = 0;
      for (const block of params.blocks) {
        const txs = block.transactions || [];
        for (const tx of txs) {
          ops += (tx.operations || []).length;
        }
      }
      console.log(
        `  blocks: ${params.blocks.length}, operations: ${ops}`
      );
    }
    if (params?.revertedBlocks) {
      console.log(`  reverting: ${params.revertedBlocks.length} blocks`);
    }
    if (params?.newBlocks) {
      console.log(`  new: ${params.newBlocks.length} blocks`);
    }

    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(
      JSON.stringify({
        jsonrpc: "2.0",
        id: parsed.id,
        result: { stateRoot: ZERO_ROOT },
      })
    );
  });
});

server.listen(port, () => {
  console.log(`mock-entitydb listening on http://localhost:${port}`);
});
