// Offline test for examples/mcp-answer-client.mjs and the `answer` command.
// Uses synthetic documents, a temporary index and a loopback model endpoint
// that records every outbound request.
// Run: RUST_RAG_BIN=/path/to/rust-rag node --test examples/mcp-answer-client.test.mjs
import assert from "node:assert/strict";
import { execFile, execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync, mkdirSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, test } from "node:test";
import { promisify } from "node:util";
import { admit, answer, mcpSearch, selectRepresentation } from "./mcp-answer-client.mjs";

const binary = process.env.RUST_RAG_BIN;
if (!binary) throw new Error("Set RUST_RAG_BIN to a rust-rag binary built with mcp,model.");
const root = mkdtempSync(join(tmpdir(), "rust-rag-mcp-answer-"));
const docs = join(root, "docs");
const stateDir = join(root, "state");
const question = "Lyra beacon relay";
const ALPHA = "Lyra beacon relay 🦀 café uses 𝟙 coil.\r\nThe relay inspection is weekly.\r\n";
const BETA = "Lyra beacon relay backup is stored in vault nine.\n";
const captured = [];
let reply = () => ({ status: "insufficient_evidence", answer: null, citations: [] });
let server;
let endpoint;

const run = (...args) => execFileSync(binary, ["--state-dir", stateDir, ...args], { encoding: "utf8" });

before(async () => {
  mkdirSync(docs);
  writeFileSync(join(docs, "alpha.md"), ALPHA);
  writeFileSync(join(docs, "beta.md"), BETA);
  run("init");
  run("ingest", docs, "--json");
  server = createServer((request, response) => {
    let body = "";
    request.on("data", (chunk) => (body += chunk));
    request.on("end", () => {
      const parsed = JSON.parse(body);
      captured.push({ url: request.url, raw: body, parsed });
      const content = JSON.parse(parsed.messages[1].content);
      response.setHeader("Content-Type", "application/json");
      response.end(
        JSON.stringify({
          choices: [{ finish_reason: "stop", message: { content: JSON.stringify(reply(content)) } }],
        }),
      );
    });
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  endpoint = `http://127.0.0.1:${server.address().port}/v1/chat/completions`;
});

after(() => {
  server?.close();
  rmSync(root, { recursive: true, force: true });
});

const ask = (options = {}) => answer({ binary, stateDir, endpoint, question, limit: 5, ...options });
const labels = (content) => content.evidence.map((item) => item.id);
const noText = (delivery) => {
  for (const entry of [...delivery.sent, ...delivery.omitted]) {
    assert.equal(entry.text, undefined);
    assert.equal(entry.passage, undefined);
  }
};

test("MCP keeps both wire forms; the client forwards one and sends each passage once", async () => {
  const result = await mcpSearch({ binary, stateDir, question, limit: 5 });
  assert.equal(typeof result.structuredContent, "object");
  assert.deepEqual(JSON.parse(result.content[0].text), result.structuredContent);
  assert.equal(selectRepresentation(result).representation, "structuredContent");
  const { content, ...textOnly } = result;
  assert.equal(selectRepresentation({ content }).representation, "content_text");
  assert.equal(textOnly.structuredContent.format, "compact");

  reply = (input) => ({ status: "answered", answer: "Weekly inspection.", citations: labels(input) });
  const before = captured.length;
  const answered = await ask();
  assert.equal(captured.length, before + 1, "exactly one model request");
  const message = captured.at(-1).parsed.messages[1].content;
  const sent = JSON.parse(message).evidence;
  assert.ok(sent.length >= 2);
  for (const item of sent) {
    assert.equal(message.split(JSON.stringify(item.passage).slice(1, -1)).length - 1, 1);
  }
  assert.ok(!message.includes("structuredContent") && !message.includes("source_hash"));
  assert.equal(answered.delivery.mcp_representation, "structuredContent");
  noText(answered.delivery);
});

test("two-source answer resolves labels and reopens exact Unicode/CRLF ranges", async () => {
  reply = (input) => ({ status: "answered", answer: "Weekly; backup in vault nine.", citations: labels(input) });
  const result = await ask();
  assert.equal(result.status, "answered", JSON.stringify(result));
  const sources = new Set(result.cited_evidence.map((entry) => entry.source_id));
  assert.equal(sources.size, 2);
  const sent = JSON.parse(captured.at(-1).parsed.messages[1].content).evidence;
  for (const entry of result.cited_evidence) {
    assert.match(entry.chunk_id, /^[0-9a-f]{64}$/);
    assert.equal(result.citations.includes(entry.chunk_id), true);
    const opened = JSON.parse(run("citation", entry.chunk_id, "--revision", entry.revision, "--json")).evidence;
    assert.equal(opened.text, sent.find((item) => item.id === entry.label).passage);
    const file = opened.path.endsWith("alpha.md") ? ALPHA : BETA;
    assert.equal([...file].slice(entry.start, entry.end).join(""), opened.text);
  }
});

test("unknown, out-of-request and canonical-ID citations are rejected", async () => {
  reply = () => ({ status: "answered", answer: "Wrong.", citations: ["E99"] });
  assert.equal((await ask()).reason, "invalid_citation");

  reply = () => ({ status: "answered", answer: "Wrong.", citations: ["E2"] });
  const narrow = await ask({ budget: { maxItems: 1 } });
  assert.equal(narrow.status, "unavailable");
  assert.equal(narrow.reason, "invalid_citation");
  assert.deepEqual(narrow.delivery.sent.map((entry) => entry.label), ["E1"]);

  const chunk = narrow.delivery.sent[0].chunk_id;
  reply = () => ({ status: "answered", answer: "Wrong.", citations: [chunk] });
  assert.equal((await ask()).reason, "invalid_citation");
});

test("omissions are reported with reasons and without passage text", async () => {
  reply = () => ({ status: "insufficient_evidence", answer: null, citations: [] });
  const limited = await ask({ budget: { maxItems: 1 } });
  assert.equal(limited.status, "insufficient_evidence");
  assert.equal(limited.delivery.sent.length, 1);
  assert.ok(limited.delivery.omitted.every((entry) => entry.reason === "item_limit"));
  assert.ok(limited.delivery.omitted.length >= 1);
  noText(limited.delivery);

  const search = selectRepresentation(await mcpSearch({ binary, stateDir, question, limit: 5 })).search;
  assert.ok(search.evidence.length >= 2);
  const smallest = Math.min(...search.evidence.map((hit) => hit.text.length));
  const budgeted = admit(question, search, { maxContext: 140 + smallest });
  const reasons = budgeted.delivery.omitted.map((entry) => entry.reason);
  assert.ok(budgeted.items.length >= 1);
  assert.ok(reasons.length >= 1);
  assert.deepEqual([...new Set(reasons)], ["context_budget"]);
  search.evidence.push({ ...search.evidence[0] });
  const all = admit(question, search);
  assert.equal(all.delivery.omitted.at(-1).reason, "duplicate");
  assert.equal(all.items.length + all.delivery.omitted.length, search.evidence.length);
  noText(all.delivery);
});

test("a retired revision withholds the answer", async () => {
  const betaPath = join(docs, "beta.md");
  reply = (input) => {
    // Edit and reindex after the passages were sent but before the reply.
    writeFileSync(betaPath, "Lyra beacon relay backup moved to vault ten.\n");
    run("ingest", docs, "--json");
    return { status: "answered", answer: "Vault nine.", citations: labels(input) };
  };
  const result = await ask();
  assert.equal(result.status, "unavailable");
  assert.equal(result.reason, "citation_unavailable");
  assert.equal(result.answer, null);
  writeFileSync(betaPath, BETA);
  run("ingest", docs, "--json");
});

test("the answer command sends each passage once and returns a resolved manifest", async () => {
  const config = join(root, "answer.json");
  writeFileSync(
    config,
    JSON.stringify({ url: endpoint, model: "local", timeout_seconds: 5, short_answer_citations: true }),
  );
  reply = (input) => ({ status: "answered", answer: "Weekly.", citations: [labels(input)[0], "E1"] });
  const before = captured.length;
  const { stdout } = await promisify(execFile)(binary, [
    "--state-dir", stateDir, "answer", question, "--model-endpoint", config, "--json",
  ]);
  assert.equal(captured.length, before + 1);
  const message = captured.at(-1).parsed.messages[1].content;
  const sent = JSON.parse(message).evidence;
  assert.deepEqual(sent.map((item) => item.id), sent.map((_, index) => `E${index + 1}`));
  for (const item of sent) {
    assert.equal(message.split(JSON.stringify(item.passage).slice(1, -1)).length - 1, 1);
  }
  const output = JSON.parse(stdout);
  assert.equal(output.status, "answered");
  assert.equal(output.delivery.label_scheme, "request_labels");
  assert.equal(output.cited_evidence.length, 1);
  assert.equal(output.cited_evidence[0].label, "E1");
  assert.equal(output.citations[0], output.cited_evidence[0].chunk_id);
  assert.ok(!JSON.stringify(output.delivery).includes(sent[0].passage));

  reply = () => ({ status: "answered", answer: "Wrong.", citations: ["E42"] });
  const rejected = await promisify(execFile)(binary, [
    "--state-dir", stateDir, "answer", question, "--model-endpoint", config, "--json",
  ]).then(
    () => assert.fail("an unknown label must fail the answer command"),
    (error) => error,
  );
  assert.notEqual(rejected.code, 0);
  const failure = JSON.parse(rejected.stdout);
  assert.equal(failure.status, "error");
  assert.deepEqual(
    failure.delivery.sent.map((entry) => entry.label),
    JSON.parse(captured.at(-1).parsed.messages[1].content).evidence.map((item) => item.id),
  );
  noText(failure.delivery);
  assert.ok(!JSON.stringify(failure).includes(sent[0].passage));
});

const closedEndpoint = async () => {
  const closed = createServer();
  await new Promise((resolve) => closed.listen(0, "127.0.0.1", resolve));
  const { port } = closed.address();
  await new Promise((resolve) => closed.close(resolve));
  return `http://127.0.0.1:${port}/v1/chat/completions`;
};

const cli = (config) =>
  promisify(execFile)(binary, [
    "--state-dir", stateDir, "answer", question, "--model-endpoint", config, "--json",
  ]).then(
    ({ stdout }) => ({ code: 0, stdout }),
    (error) => ({ code: error.code, stdout: error.stdout }),
  );

test("the answer command withholds a retired citation with its reason and a nonzero exit", async () => {
  const config = join(root, "answer.json");
  writeFileSync(
    config,
    JSON.stringify({ url: endpoint, model: "local", timeout_seconds: 5, short_answer_citations: true }),
  );
  const betaPath = join(docs, "beta.md");
  reply = (input) => {
    writeFileSync(betaPath, "Lyra beacon relay backup moved to vault ten.\n");
    run("ingest", docs, "--json");
    return { status: "answered", answer: "Vault nine.", citations: labels(input) };
  };
  try {
    const { code, stdout } = await cli(config);
    assert.notEqual(code, 0);
    const output = JSON.parse(stdout);
    assert.equal(output.status, "unavailable");
    assert.equal(output.reason, "citation_unavailable");
    assert.equal(output.answer, null);
    assert.deepEqual(output.citations, []);
    assert.deepEqual(output.cited_evidence, []);
    assert.equal(output.answer_generated, false);
    assert.ok(output.delivery.sent.length >= 2);
    noText(output.delivery);
  } finally {
    writeFileSync(betaPath, BETA);
    run("ingest", docs, "--json");
  }
});

const sqlite = await import("node:sqlite").catch(() => null);

test(
  "a locked index after the model call keeps the delivery manifest",
  { skip: sqlite ? false : "node:sqlite is unavailable in this Node.js version" },
  async () => {
    const config = join(root, "answer.json");
    let lock;
    reply = (input) => {
      // Lock the index after the passages were sent so the recheck fails.
      lock = new sqlite.DatabaseSync(join(stateDir, "personal.sqlite3"));
      lock.exec("BEGIN EXCLUSIVE");
      return { status: "answered", answer: "Weekly.", citations: labels(input) };
    };
    try {
      const { code, stdout } = await cli(config);
      assert.notEqual(code, 0);
      const failure = JSON.parse(stdout);
      assert.equal(failure.status, "error");
      assert.ok(failure.delivery.sent.length >= 1);
      noText(failure.delivery);
    } finally {
      lock?.exec("ROLLBACK");
      lock?.close();
    }
  },
);

test("an unreachable model still reports what was prepared for the request", async () => {
  const unreachable = await closedEndpoint();
  const config = join(root, "closed.json");
  writeFileSync(
    config,
    JSON.stringify({ url: unreachable, model: "local", timeout_seconds: 5, short_answer_citations: true }),
  );
  const { code, stdout } = await cli(config);
  assert.notEqual(code, 0);
  const failure = JSON.parse(stdout);
  assert.equal(failure.status, "error");
  assert.ok(failure.delivery.sent.length >= 1);
  noText(failure.delivery);

  const result = await answer({ binary, stateDir, endpoint: unreachable, question, limit: 5 });
  assert.equal(result.status, "unavailable");
  assert.equal(result.reason, "model_unavailable");
  assert.ok(result.delivery.sent.length >= 1);
  noText(result.delivery);
});

test("a model that stalls mid-body is unavailable, not an invalid response", async () => {
  const stalled = createServer((request, response) => {
    request.resume();
    response.writeHead(200, { "Content-Type": "application/json" });
    response.write('{"choices":[');
  });
  await new Promise((resolve) => stalled.listen(0, "127.0.0.1", resolve));
  try {
    const result = await answer({
      binary,
      stateDir,
      endpoint: `http://127.0.0.1:${stalled.address().port}/v1/chat/completions`,
      question,
      limit: 5,
      timeoutMs: 3_000,
    });
    assert.equal(result.status, "unavailable");
    assert.equal(result.reason, "model_unavailable");
    assert.ok(result.delivery.sent.length >= 1);
  } finally {
    stalled.closeAllConnections();
    stalled.close();
  }
});

test("an MCP search that outlives its deadline fails instead of hanging", async () => {
  await assert.rejects(mcpSearch({ binary, stateDir, question, timeoutMs: 1 }), /timed out/);
});

test("a search that stops early or never starts leaves no stray pipe error", async () => {
  const stray = [];
  const record = (error) => stray.push(error);
  process.on("uncaughtException", record);
  try {
    for (let round = 0; round < 40; round++) {
      // The deadline kills the server while the client's first write may still be pending.
      await assert.rejects(
        mcpSearch({ binary, stateDir, question, timeoutMs: round % 2 }),
        /timed out|exited with code|EPIPE|EOF/,
      );
    }
    await assert.rejects(
      mcpSearch({ binary: process.execPath, stateDir, question, timeoutMs: 5_000 }),
      /exited with code|EPIPE|EOF/,
    );
    await assert.rejects(
      mcpSearch({ binary: join(stateDir, "missing-rust-rag"), stateDir, question, timeoutMs: 5_000 }),
      /ENOENT/,
    );
    await new Promise((resolve) => setTimeout(resolve, 50));
  } finally {
    process.off("uncaughtException", record);
  }
  assert.deepEqual(stray.map(String), []);
});
