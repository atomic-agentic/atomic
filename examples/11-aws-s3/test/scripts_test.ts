// Runs the write script the way Atomic is planned to run it: a clean
// environment holding only what the atom declares, and Deno permissions built
// from the atom's declared network hosts. S3 is a local stand-in, so the tests
// need no AWS account and send nothing off this machine.
//
//   deno test -A --lock=deno.lock --frozen test/
//
// The test harness itself is trusted (-A). The script under test is not.
import { FakeS3 } from "./fake_s3.ts";

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
