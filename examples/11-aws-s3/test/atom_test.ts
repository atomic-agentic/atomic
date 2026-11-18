// Runs the TypeScript block of atoms/summarize-report.atom.astro the way Atomic
// is planned to run it. S3 is a local stand-in; nothing leaves this machine.
//
//   deno test -A --lock=deno.lock --frozen test/
import { check, runPrepare } from "./atom_host.ts";
import { FakeS3 } from "./fake_s3.ts";

const ATOM = "atoms/summarize-report.atom.astro";
const BUCKET = "acme-reports";
const root = new URL("../", import.meta.url);
const readJson = async (path: string) =>
  JSON.parse(await Deno.readTextFile(new URL(path, root)));

function equal(actual: unknown, expected: unknown, what: string) {
  const [a, e] = [JSON.stringify(actual), JSON.stringify(expected)];
  if (a !== e) throw new Error(`${what}\n  expected ${e}\n  actual   ${a}`);
}

async function withReport(test: (s3: FakeS3, host: string) => Promise<void>) {
  const s3 = new FakeS3();
  try {
    const prepared = await readJson(
      "fixtures/summarize-report/october.prepared.json",
    );
    await s3.put(BUCKET, "reports/2026-10.md", prepared.report.text);
    await test(s3, new URL(s3.endpoint).host);
  } finally {
    await s3.close();
  }
}

Deno.test("the fetch script type-checks, and the Astro prompt reads only its output", async () => {
  // The committed sample must also have the shape prepare returns.
  const sample = await readJson(
    "fixtures/summarize-report/october.prepared.json",
  );
  const result = await check(ATOM, { sample });
  if (result.code !== 0) throw new Error(result.stderr);
});

Deno.test("a path the fetch script does not return is a type error", async () => {
  const result = await check(ATOM, { extraPaths: ["report.body"] });
  if (result.code === 0) throw new Error("expected a type error");
  if (!result.stderr.includes("body")) throw new Error(result.stderr);
});

Deno.test("the workflow binds and displays only fields returned by the fetch step", async () => {
  // A workflow may read steps.<id>.prepared.<path>, for example to show a gate
  // the hash of what the model saw. Those paths are checked the same way.
  const workflow = await Deno.readTextFile(
    new URL("workflows/digest-report.workflow.yaml", root),
  );
  const paths = [
    ...workflow.matchAll(/steps\.fetch\.output((?:\.\w+)+)/g),
  ].map((m) => m[1].slice(1));
  equal(paths, ["report", "report.sha256"], "prepared paths the workflow reads");
  const result = await check(ATOM, { extraPaths: paths });
  if (result.code !== 0) throw new Error(result.stderr);
});

Deno.test("a sample that does not match the fetch output is a type error", async () => {
  const result = await check(ATOM, {
    sample: { report: { text: "x", bytes: "one" } },
  });
  if (result.code === 0) throw new Error("expected a type error");
});

Deno.test("the fetch script reads the report and returns the fixture", () =>
  withReport(async (s3, host) => {
    const input = await readJson(
      "fixtures/fetch-report/october.input.json",
    );
    const expected = await readJson(
      "fixtures/summarize-report/october.prepared.json",
    );
    const result = await runPrepare(ATOM, input, {
      allowNet: host,
      endpoint: s3.endpoint,
    });
    equal(result.code, 0, result.stderr);
    equal(result.prepared, expected, "prepared values");
    equal(result.ran, ["fetch"], "steps that ran");
    equal(s3.requests, ["GET acme-reports/reports/2026-10.md"], "requests");
  }));

Deno.test("a replay uses the saved step and needs no network", () =>
  withReport(async (s3, host) => {
    const input = await readJson(
      "fixtures/fetch-report/october.input.json",
    );
    const first = await runPrepare(ATOM, input, {
      allowNet: host,
      endpoint: s3.endpoint,
    });
    equal(first.code, 0, first.stderr);

    // Same input, saved step results, and no network permission at all.
    const replay = await runPrepare(ATOM, input, {
      endpoint: s3.endpoint,
      checkpoints: first.checkpoints,
    });
    equal(replay.code, 0, replay.stderr);
    equal(replay.prepared, first.prepared, "prepared values on replay");
    equal(replay.ran, [], "steps that ran on replay");
    equal(s3.requests.length, 1, "requests after the replay");
  }));

Deno.test("the atom's code cannot reach a host the atom did not declare", () =>
  withReport(async (s3) => {
    const input = await readJson(
      "fixtures/fetch-report/october.input.json",
    );
    const result = await runPrepare(ATOM, input, {
      allowNet: "127.0.0.1:1",
      endpoint: s3.endpoint,
    });
    if (result.code === 0) throw new Error("expected a failure");
    if (!result.stderr.includes("Requires net access")) {
      throw new Error(result.stderr);
    }
    equal(s3.requests, [], "requests that reached the endpoint");
  }));

Deno.test("with no endpoint override, the SDK asks for the bucket's own host", async () => {
  // Nothing is sent: the only allowed host is unreachable, and Deno names the
  // host the SDK wanted. That name is what the atom's network.allow holds.
  const input = await readJson("fixtures/fetch-report/october.input.json");
  const result = await runPrepare(ATOM, input, { allowNet: "127.0.0.1:1" });
  if (result.code === 0) throw new Error("expected a failure");
  if (!result.stderr.includes(`${BUCKET}.s3.us-east-1.amazonaws.com`)) {
    throw new Error(result.stderr);
  }
});
