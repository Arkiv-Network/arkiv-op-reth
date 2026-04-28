#!/usr/bin/env node
// Mock EntityDB — pretty-prints incoming JSON-RPC requests and returns a
// dummy state root.
//
// Usage: node scripts/mock-entitydb.js [port]
//        RAW=1 node scripts/mock-entitydb.js   # dump raw JSON instead

const http = require("http");
const port = process.argv[2] || 9545;
const RAW = process.env.RAW === "1";
const ZERO_ROOT =
  "0x0000000000000000000000000000000000000000000000000000000000000000";

let totals = { blocks: 0, txs: 0, ops: 0, requests: 0 };

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

function shortHex(s) {
  if (typeof s !== "string" || !s.startsWith("0x")) return String(s);
  if (s.length <= 12) return s;
  return `${s.slice(0, 8)}…${s.slice(-4)}`;
}

function payloadLen(hex) {
  if (typeof hex !== "string" || !hex.startsWith("0x")) return 0;
  return Math.max(0, (hex.length - 2) / 2);
}

function fmtAttrs(attrs) {
  if (!Array.isArray(attrs) || attrs.length === 0) return "";
  const counts = { string: 0, numeric: 0, entityKey: 0 };
  for (const a of attrs) {
    if (a.stringValue !== undefined) counts.string++;
    else if (a.numericValue !== undefined) counts.numeric++;
    else if (a.entityKey !== undefined) counts.entityKey++;
  }
  const parts = [];
  if (counts.string) parts.push(`${counts.string}s`);
  if (counts.numeric) parts.push(`${counts.numeric}n`);
  if (counts.entityKey) parts.push(`${counts.entityKey}k`);
  return `attrs=${attrs.length}(${parts.join("/")})`;
}

function fmtOp(op, idx) {
  const type = op.type;
  const fields = [`key=${shortHex(op.entityKey)}`];

  if (op.owner) fields.push(`owner=${shortHex(op.owner)}`);
  if (op.expiresAt !== undefined) fields.push(`expiresAt=${op.expiresAt}`);
  if (op.payload !== undefined) fields.push(`payload=${payloadLen(op.payload)}B`);
  if (op.contentType) fields.push(`ct=${op.contentType}`);
  if (op.attributes) {
    const a = fmtAttrs(op.attributes);
    if (a) fields.push(a);
  }
  if (op.changesetHash) fields.push(`csh=${shortHex(op.changesetHash)}`);

  return `        [${idx}] ${type.padEnd(8)} ${fields.join("  ")}`;
}

function fmtTx(tx) {
  const lines = [
    `      tx ${shortHex(tx.hash)}  index=${tx.index}  sender=${shortHex(tx.sender)}`,
  ];
  for (let i = 0; i < tx.operations.length; i++) {
    lines.push(fmtOp(tx.operations[i], i));
  }
  return lines.join("\n");
}

function fmtBlock(block) {
  const h = block.header;
  const txs = block.transactions || [];
  const txCount = txs.length;
  const opCount = txs.reduce((n, t) => n + (t.operations?.length || 0), 0);

  const headerLine =
    `    block #${h.number} ${shortHex(h.hash)}` +
    `  parent=${shortHex(h.parentHash)}` +
    `  csh=${shortHex(h.changesetHash)}` +
    (txCount === 0 ? "  (empty)" : `  txs=${txCount} ops=${opCount}`);

  const lines = [headerLine];
  for (const tx of txs) lines.push(fmtTx(tx));
  return lines.join("\n");
}

function fmtBlockRef(ref) {
  return `    #${ref.number} ${shortHex(ref.hash)}`;
}

// ---------------------------------------------------------------------------
// Per-method renderers
// ---------------------------------------------------------------------------

function renderCommit(blocks) {
  const txCount = blocks.reduce(
    (n, b) => n + (b.transactions?.length || 0),
    0
  );
  const opCount = blocks.reduce(
    (n, b) =>
      n +
      (b.transactions || []).reduce(
        (m, t) => m + (t.operations?.length || 0),
        0
      ),
    0
  );
  totals.blocks += blocks.length;
  totals.txs += txCount;
  totals.ops += opCount;

  const header = `=== arkiv_commitChain  blocks=${blocks.length} txs=${txCount} ops=${opCount}`;
  console.log(header);
  for (const b of blocks) console.log(fmtBlock(b));
}

function renderRevert(refs) {
  console.log(`=== arkiv_revert  blocks=${refs.length}`);
  for (const r of refs) console.log(fmtBlockRef(r));
}

function renderReorg(reverted, newBlocks) {
  console.log(
    `=== arkiv_reorg  reverted=${reverted.length} new=${newBlocks.length}`
  );
  if (reverted.length > 0) {
    console.log("  reverted:");
    for (const r of reverted) console.log(fmtBlockRef(r));
  }
  if (newBlocks.length > 0) {
    console.log("  new:");
    for (const b of newBlocks) console.log(fmtBlock(b));
  }

  // Track the newBlocks portion in totals.
  const txCount = newBlocks.reduce(
    (n, b) => n + (b.transactions?.length || 0),
    0
  );
  const opCount = newBlocks.reduce(
    (n, b) =>
      n +
      (b.transactions || []).reduce(
        (m, t) => m + (t.operations?.length || 0),
        0
      ),
    0
  );
  totals.blocks += newBlocks.length;
  totals.txs += txCount;
  totals.ops += opCount;
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

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

    totals.requests++;
    const method = parsed.method;
    const params = parsed.params?.[0] || {};

    if (RAW) {
      console.log(`\n=== ${method} ===`);
      console.log(JSON.stringify(params, null, 2));
    } else {
      console.log("");
      switch (method) {
        case "arkiv_commitChain":
          renderCommit(params.blocks || []);
          break;
        case "arkiv_revert":
          renderRevert(params.blocks || []);
          break;
        case "arkiv_reorg":
          renderReorg(
            params.revertedBlocks || [],
            params.newBlocks || []
          );
          break;
        default:
          console.log(`=== ${method} ===`);
          console.log(JSON.stringify(params, null, 2));
      }
      console.log(
        `  totals: requests=${totals.requests} blocks=${totals.blocks} txs=${totals.txs} ops=${totals.ops}`
      );
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
  if (RAW) console.log("(RAW=1 set: dumping unformatted JSON)");
});
