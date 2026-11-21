// Runs a script the way Atomic is planned to run it: a clean environment
// holding only what the atom declares, and Deno permissions built from the
// atom's declared network hosts. S3 is a local stand-in, so the tests need no
// AWS account and send nothing off this machine.
//
//   deno test -A --lock=deno.lock --frozen test/
//
// The test harness itself is trusted (-A). The script under test is not.
import { MAX_BYTES } from "../tools/fetch_report.ts";
import { FakeS3, type Transfer } from "./fake_s3.ts";

const BUCKET = "acme-reports";
const root = new URL("../", import.meta.url);
const readJson = async (path: string) =>
  JSON.parse(await Deno.readTextFile(new URL(path, root)));

function equal(actual: unknown, expected: unknown, what: string) {
  const [a, e] = [JSON.stringify(actual), JSON.stringify(expected)];
  if (a !== e) throw new Error(`${what}\n  expected ${e}\n  actual   ${a}`);
}

interface Result {
  code: number;
  output: unknown;
  stderr: string;
}

/** `allowNet` is what the atom's `network.allow` would hold. */
async function run(
  script: string,
  input: unknown,
  allowNet: string,
  endpoint?: string,
): Promise<Result> {
  const env: Record<string, string> = {
    AWS_REGION: "us-east-1",
    REPORTS_BUCKET: BUCKET,
    // What the credential broker would inject: short-lived keys for the role.
    AWS_ACCESS_KEY_ID: "test-key-id",
    AWS_SECRET_ACCESS_KEY: "test-secret",
    AWS_SESSION_TOKEN: "test-token",
    NO_COLOR: "1",
  };
  const declared = Object.keys(env).filter((name) => name !== "NO_COLOR");
  // The step's scratch directory: the only place the code may write, and HOME.
  const scratch = await Deno.makeTempDir({ prefix: "atomic-scratch-" });
  env.HOME = scratch;
  const cache = Deno.env.get("DENO_DIR"); // where Deno keeps its package cache
  if (cache) env.DENO_DIR = cache;
  if (endpoint) { // standard SDK setting; tests only
    env.AWS_ENDPOINT_URL_S3 = endpoint;
    declared.push("AWS_ENDPOINT_URL_S3");
  }
  const child = new Deno.Command(Deno.execPath(), {
    args: [
      "run",
      "--no-prompt", // a missing permission is an error, never a question
      "--lock=deno.lock",
      "--frozen", // the lockfile must already match
      "--cached-only", // packages were fetched in the setup step
      `--allow-net=${allowNet}`,
      // Exactly the variables the atom declares, plus HOME, which Atomic points
      // at the scratch directory. Any other lookup returns undefined.
      `--allow-env=${[...declared, "HOME"].join(",")}`,
      "--ignore-env",
      `--allow-read=${root.pathname}`, // the project, read-only
      `--allow-write=${scratch}`, // the scratch directory
      "--no-remote", // no modules from URLs; packages come from the lockfile
      "--allow-sys=osRelease", // the SDK puts the OS version in its user agent
      script,
    ],
    cwd: root,
    clearEnv: true,
    env,
    stdin: "piped",
    stdout: "piped",
    stderr: "piped",
  }).spawn();
  const writer = child.stdin.getWriter();
  await writer.write(new TextEncoder().encode(JSON.stringify(input)));
  await writer.close();
  const { code, stdout, stderr } = await child.output();
  await Deno.remove(scratch, { recursive: true });
  const text = new TextDecoder().decode(stdout).trim();
  return {
    code,
    output: text ? JSON.parse(text) : null,
    stderr: new TextDecoder().decode(stderr),
  };
}

async function withFake(test: (s3: FakeS3, host: string) => Promise<void>) {
  const s3 = new FakeS3();
  try {
    await test(s3, new URL(s3.endpoint).host);
  } finally {
    await s3.close();
  }
}

Deno.test("put-digest writes once, and a repeat of the same step is safe", () =>
  withFake(async (s3, host) => {
    const input = await readJson("fixtures/put-digest/october.input.json");
    const expected = await readJson("fixtures/put-digest/october.output.json");

    const first = await run("tools/put_object.ts", input, host, s3.endpoint);
    equal(first.code, 0, first.stderr);
    equal(first.output, expected, "first attempt");

    // The same step again, as after a crash between the write and the checkpoint.
    const second = await run("tools/put_object.ts", input, host, s3.endpoint);
    equal(second.code, 0, second.stderr);
    equal(second.output, { ...expected, created: false }, "second attempt");
    equal(s3.objects.size, 1, "objects stored");
    equal(
      new TextDecoder().decode(s3.objects.get(`${BUCKET}/${input.key}`)!.body),
      input.text,
      "stored text",
    );
  }));

Deno.test("put-digest refuses to replace different content", () =>
  withFake(async (s3, host) => {
    const input = await readJson("fixtures/put-digest/october.input.json");
    await s3.put(BUCKET, input.key, "an earlier digest");
    const result = await run("tools/put_object.ts", input, host, s3.endpoint);
    if (result.code === 0) throw new Error("expected a failure");
    if (!result.stderr.includes("already exists with different content")) {
      throw new Error(result.stderr);
    }
    equal(
      new TextDecoder().decode(s3.objects.get(`${BUCKET}/${input.key}`)!.body),
      "an earlier digest",
      "stored text",
    );
  }));

Deno.test("a script cannot reach a host the atom did not declare", () =>
  withFake(async (s3) => {
    // The endpoint is real and listening, but it is not in the allowed list.
    const result = await run(
      "tools/put_object.ts",
      { key: "digests/2026-10.md", text: "x" },
      "127.0.0.1:1",
      s3.endpoint,
    );
    if (result.code === 0) throw new Error("expected a failure");
    if (!result.stderr.includes("Requires net access")) {
      throw new Error(result.stderr);
    }
    equal(s3.requests, [], "requests that reached the endpoint");
  }));

// The size limit for one report, seen from the server side. A report bigger
// than the limit has to be refused while its body arrives: a check after the
// whole body had been read would still let one oversized object fill the step.
const report = "reports/2026-11.md";
const fetchReport = async (s3: FakeS3, host: string) =>
  await run("tools/fetch_report.ts", { key: report }, host, s3.endpoint);
const tooBig = (s3: FakeS3) => s3.transfers.get(`${BUCKET}/${report}`)!;

/** The digest fetch-report is expected to report for these bytes. */
const sha256 = async (text: string) => {
  const hash = new Uint8Array(
    await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text)),
  );
  return Array.from(hash, (b) => b.toString(16).padStart(2, "0")).join("");
};

/**
 * The transfer for this GET, once the fake has seen how it ended or two
 * seconds have gone by. The client going away reaches the fake asynchronously.
 */
const settled = async (s3: FakeS3): Promise<Transfer> => {
  for (let waited = 0; ; waited += 25) {
    const transfer = tooBig(s3);
    if (transfer.closedEarly || transfer.finished || waited > 2000) {
      return transfer;
    }
    await new Promise((r) => setTimeout(r, 25));
  }
};

Deno.test("fetch-report reads a report of exactly the size limit", () =>
  withFake(async (s3, host) => {
    const text = "x".repeat(MAX_BYTES);
    await s3.put(BUCKET, report, text);
    const result = await fetchReport(s3, host);
    equal(result.code, 0, result.stderr);
    equal(
      result.output,
      { report: { text, bytes: MAX_BYTES, sha256: await sha256(text) } },
      "fetched report",
    );
  }));

Deno.test("fetch-report reassembles a report that arrives in pieces", () =>
  withFake(async (s3, host) => {
    // Exactly the limit again, but undeclared and in small pieces, so the
    // bytes only come back whole if every piece was kept in order.
    const text = "x".repeat(MAX_BYTES);
    s3.stream(BUCKET, report, {
      total: MAX_BYTES,
      chunkBytes: 4096,
      contentLength: false,
    });
    const result = await fetchReport(s3, host);
    equal(result.code, 0, result.stderr);
    equal(
      result.output,
      { report: { text, bytes: MAX_BYTES, sha256: await sha256(text) } },
      "reassembled report",
    );
  }));

Deno.test("fetch-report refuses an over-limit report without reading its body", () =>
  withFake(async (s3, host) => {
    // Well over the limit, length declared, and no byte on its way until
    // 250 ms in: the refusal comes from the declared length while the body
    // has only just begun, so a step that read any of it would show it had.
    s3.stream(BUCKET, report, {
      total: 10 * 1024 * 1024,
      chunkBytes: 8192,
      firstDelayMs: 250,
      chunkDelayMs: 5,
    });
    const started = Date.now();
    const result = await fetchReport(s3, host);
    const seconds = (Date.now() - started) / 1000;
    if (result.code === 0) throw new Error("expected a failure");
    if (!result.stderr.includes(`${report} is larger than ${MAX_BYTES} bytes`)) {
      throw new Error(result.stderr);
    }
    // The declared length answers the refusal; waiting for the first byte
    // would be reading the body after all.
    if (seconds > 1) {
      throw new Error(`returned only after ${seconds.toFixed(1)}s`);
    }
    const transfer = await settled(s3);
    if (transfer.finished || !transfer.closedEarly) {
      throw new Error(`read the body instead of its length: ${JSON.stringify(transfer)}`);
    }
  }));

Deno.test("fetch-report refuses an over-limit report of no declared length partway through", () =>
  withFake(async (s3, host) => {
    // 10 MB with no Content-Length, so only counting the bytes as they
    // arrive can stop this. The small pace is what makes the early close
    // deterministic: without it the whole body sits in socket buffers before
    // the step has read past the cap, and the fake has nothing left to stop.
    s3.stream(BUCKET, report, {
      total: 10 * 1024 * 1024,
      chunkBytes: 65_536,
      chunkDelayMs: 5,
      contentLength: false,
    });
    const result = await fetchReport(s3, host);
    if (result.code === 0) throw new Error("expected a failure");
    if (!result.stderr.includes(`${report} is larger than ${MAX_BYTES} bytes`)) {
      throw new Error(result.stderr);
    }
    const transfer = await settled(s3);
    // Not finished is the point: of the 10 MB on offer, the step stopped
    // reading partway through. How far the server got before the cancel
    // lands is the socket's, not the step's, so it is not asserted here.
    if (transfer.finished || !transfer.closedEarly) {
      throw new Error(`read the whole body: ${JSON.stringify(transfer)}`);
    }
  }));

Deno.test("fetch-report answers while an over-limit report is still arriving", () =>
  withFake(async (s3, host) => {
    // 10 MB with no Content-Length, a chunk every 10 ms: over ten seconds of
    // sending if it is read to the end, and no declared length to refuse
    // from, so the step has to stop partway through a body that is arriving.
    s3.stream(BUCKET, report, {
      total: 10 * 1024 * 1024,
      chunkBytes: 8192,
      chunkDelayMs: 10,
      contentLength: false,
    });
    const started = Date.now();
    const result = await fetchReport(s3, host);
    const seconds = (Date.now() - started) / 1000;
    if (result.code === 0) throw new Error("expected a failure");
    if (!result.stderr.includes(`${report} is larger than ${MAX_BYTES} bytes`)) {
      throw new Error(result.stderr);
    }
    if (seconds > 5) {
      throw new Error(`returned only after ${seconds.toFixed(1)}s`);
    }
    const transfer = await settled(s3);
    if (transfer.finished) {
      throw new Error(`the fetch outlived the body: ${JSON.stringify(transfer)}`);
    }
    if (!transfer.closedEarly) {
      throw new Error(`the fake never saw the read stop: ${JSON.stringify(transfer)}`);
    }
    // Sending is the slow side here, so how far it got says where the reader
    // gave up: about the cap, not far into the 10 MB. Whether the transfer
    // ended with that refusal is not visible from here, because the child
    // exits either way; test/fetch_report_test.ts refuses from a process that
    // stays alive, and is what checks that.
    if (transfer.sent > 3 * MAX_BYTES) {
      throw new Error(`the read went far past the cap: ${JSON.stringify(transfer)}`);
    }
  }));
