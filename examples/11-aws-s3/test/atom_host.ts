// A stand-in for the part of Atomic that runs the code in an atom file.
//
// The Astro prompt reads the output of tools/fetch_report.ts as ordinary input.
// This test-only harness simulates checkpoint reuse around that standalone script.
const root = new URL("../", import.meta.url);

export interface PrepareResult {
  code: number;
  prepared: unknown;
  /** Saved step results, as Atomic would checkpoint them. */
  checkpoints: Record<string, unknown>;
  /** Names of the steps whose code actually ran. */
  ran: string[];
  stderr: string;
}

const ENTRY = `
import * as atom from "./atom.ts";
const { input, checkpoints } = JSON.parse(await new Response(Deno.stdin.readable).text());
const saved: Record<string, unknown> = { ...checkpoints };
const ran: string[] = [];
const ctx = {
  async step<T>(name: string, run: () => Promise<T>): Promise<T> {
    if (name in saved) return saved[name] as T; // replay: the code does not run
    ran.push(name);
    return (saved[name] = await run()) as T;
  },
};
const prepared = await ctx.step("fetch", () => atom.fetchReport(input));
console.log(JSON.stringify({ prepared, checkpoints: saved, ran }));
`;

/** Paths read by the Astro prompt from the fetched report. */
export function preparedPaths(body: string): string[] {
  return [...body.matchAll(/\{input((?:\.\w+)+)\}/g)].map((m) => m[1].slice(1));
}

/**
 * A module that only type-checks. Each report path in the Astro instructions
 * becomes a property access on the type fetchReport() returns, so a misspelled
 * path is a TypeScript error. A sample value is checked against the same type.
 */
function pathsModule(paths: string[], sample?: unknown): string {
  return [
    'import type * as atom from "./atom.ts";',
    "type Prepared = Awaited<ReturnType<typeof atom.fetchReport>>;",
    "declare const prepared: Prepared;",
    ...paths.map((path) => `void prepared.${path};`),
    ...(sample === undefined ? [] : [
      `const sample: Prepared = ${JSON.stringify(sample)};`,
      "void sample;",
    ]),
  ].join("\n") + "\n";
}

export interface GenerateOptions {
  /** Extra fetched-report paths to check, beyond those in the instructions. */
  extraPaths?: string[];
  /** A value to check against the type fetchReport() returns. */
  sample?: unknown;
}

/** Writes the generated modules to a fresh directory and returns it. */
export async function generate(
  atomPath: string,
  options: GenerateOptions = {},
): Promise<string> {
  const code = await Deno.readTextFile(new URL("tools/fetch_report.ts", root));
  const body = await Deno.readTextFile(new URL(atomPath, root));
  const dir = await Deno.makeTempDir({ prefix: "atomic-atom-" });
  await Deno.writeTextFile(`${dir}/atom.ts`, code + "\n");
  await Deno.writeTextFile(`${dir}/entry.ts`, ENTRY);
  await Deno.writeTextFile(
    `${dir}/paths.ts`,
    pathsModule(
      [...preparedPaths(body), ...(options.extraPaths ?? [])],
      options.sample,
    ),
  );
  await Deno.writeTextFile(
    `${dir}/import_map.json`,
    JSON.stringify({
      imports: {
        "atomic:std": new URL("test/atomic/std.ts", root).href,
        "atomic:types": new URL("test/atomic/types.ts", root).href,
      },
    }),
  );
  return dir;
}

/** The flags that pin packages: the same for a check and for a run. */
export const pinned = (dir: string) => [
  "--lock=deno.lock",
  "--frozen", // the lockfile must already match
  "--cached-only", // packages were fetched in the setup step
  `--import-map=${dir}/import_map.json`,
];

/** Type-checks the generated modules, as `atomic check` is planned to. */
export async function check(
  atomPath: string,
  options: GenerateOptions = {},
): Promise<{ code: number; stderr: string }> {
  const dir = await generate(atomPath, options);
  const result = await new Deno.Command(Deno.execPath(), {
    args: ["check", ...pinned(dir), `${dir}/entry.ts`, `${dir}/paths.ts`],
    cwd: root,
    env: { NO_COLOR: "1" },
    stdout: "piped",
    stderr: "piped",
  }).output();
  await Deno.remove(dir, { recursive: true });
  return { code: result.code, stderr: new TextDecoder().decode(result.stderr) };
}

export interface RunOptions {
  /** What the atom's `network.allow` holds. Omit for no network at all. */
  allowNet?: string;
  /** Test-only S3 endpoint. */
  endpoint?: string;
  checkpoints?: Record<string, unknown>;
}

export async function runPrepare(
  atomPath: string,
  input: unknown,
  options: RunOptions = {},
): Promise<PrepareResult> {
  const dir = await generate(atomPath);
  const env: Record<string, string> = {
    AWS_REGION: "us-east-1",
    REPORTS_BUCKET: "acme-reports",
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
  if (options.endpoint) { // standard SDK setting; tests only
    env.AWS_ENDPOINT_URL_S3 = options.endpoint;
    declared.push("AWS_ENDPOINT_URL_S3");
  }
  const child = new Deno.Command(Deno.execPath(), {
    args: [
      "run",
      "--no-prompt", // a missing permission is an error, never a question
      ...pinned(dir),
      ...(options.allowNet ? [`--allow-net=${options.allowNet}`] : []),
      // Exactly the variables the atom declares, plus HOME, which Atomic points
      // at the scratch directory. Any other lookup returns undefined.
      `--allow-env=${[...declared, "HOME"].join(",")}`,
      "--ignore-env",
      `--allow-read=${root.pathname}`, // the project, read-only
      `--allow-write=${scratch}`, // the scratch directory
      "--no-remote", // no modules from URLs; packages come from the lockfile
      "--allow-sys=osRelease", // the SDK puts the OS version in its user agent
      `${dir}/entry.ts`,
    ],
    cwd: root,
    clearEnv: true,
    env,
    stdin: "piped",
    stdout: "piped",
    stderr: "piped",
  }).spawn();
  const writer = child.stdin.getWriter();
  await writer.write(
    new TextEncoder().encode(
      JSON.stringify({ input, checkpoints: options.checkpoints ?? {} }),
    ),
  );
  await writer.close();
  const { code, stdout, stderr } = await child.output();
  await Deno.remove(dir, { recursive: true });
  await Deno.remove(scratch, { recursive: true });
  const text = new TextDecoder().decode(stdout).trim();
  const result = text
    ? JSON.parse(text)
    : { prepared: null, checkpoints: {}, ran: [] };
  return { code, ...result, stderr: new TextDecoder().decode(stderr) };
}
