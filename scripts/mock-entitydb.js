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
        result: { stateRoot: ZERO_ROOT },
      })
    );
  });
});

server.listen(port, () => {
  console.log(`mock-entitydb listening on http://localhost:${port}`);
});
