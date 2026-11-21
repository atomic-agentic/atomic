// The two refusals of an over-limit report, from inside the test process.
//
//   deno test -A --lock=deno.lock --frozen test/
//
// A step that ran in a child process hands its open sockets to the kernel when
// it exits, so a child-process test cannot tell whether the fetch ended its
// transfer or the process simply ended (test/scripts_test.ts covers what that
// step says and does). Here nothing else could have closed the connection:
// fetch_report is imported and called the way the runtime calls it, and this
// process stays alive while the fake is watched.
import { fetchReport, MAX_BYTES } from "../tools/fetch_report.ts";
import { FakeS3, type Transfer } from "./fake_s3.ts";

const BUCKET = "acme-reports";
const report = "reports/2026-11.md";
const transfer = (s3: FakeS3): Transfer =>
  s3.transfers.get(`${BUCKET}/${report}`)!;

async function withFake(test: (s3: FakeS3) => Promise<void>) {
  const s3 = new FakeS3();
  try {
    await test(s3);
  } finally {
    await s3.close();
  }
}

/**
 * Points this process's environment at the fake, as the child-process harness
 * points a child at it, and returns the function that puts it back.
 */
const useFake = (s3: FakeS3) => {
  const names = [
    "AWS_REGION",
    "REPORTS_BUCKET",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_ENDPOINT_URL_S3", // standard SDK setting; tests only
  ];
  const saved = names.map((name) => [name, Deno.env.get(name)] as const);
  Deno.env.set("AWS_REGION", "us-east-1");
  Deno.env.set("REPORTS_BUCKET", BUCKET);
  // What the credential broker would inject: short-lived keys for the role.
  Deno.env.set("AWS_ACCESS_KEY_ID", "test-key-id");
  Deno.env.set("AWS_SECRET_ACCESS_KEY", "test-secret");
  Deno.env.set("AWS_SESSION_TOKEN", "test-token");
  Deno.env.set("AWS_ENDPOINT_URL_S3", s3.endpoint);
  return () => {
    for (const [name, value] of saved) {
      if (value === undefined) Deno.env.delete(name);
      else Deno.env.set(name, value);
    }
  };
};

/** What one fetchReport call did, and how long it took. */
const refuse = async (key: string) => {
  const started = Date.now();
  try {
    await fetchReport({ key });
    return { ms: Date.now() - started, message: undefined };
  } catch (error) {
    return { ms: Date.now() - started, message: (error as Error).message };
  }
};

/**
 * Says that this GET's transfer ended with the refusal that just happened, and
 * not later or not at all, and returns what the fake saw. The two clocks start
 * together — one when the call is made, one when its request reaches the fake —
 * so they are comparable, with a second left for scheduling. Nothing here has
 * exited, so the close the fake sees can only be the fetch's own.
 */
const assertEnded = async (s3: FakeS3, refusedAfterMs: number): Promise<Transfer> => {
  let seen = transfer(s3);
  for (let waited = 0; !seen.closedEarly && waited <= 1_000; waited += 10) {
    await new Promise((r) => setTimeout(r, 10));
    seen = transfer(s3);
  }
  if (!seen.closedEarly) {
    throw new Error(`left its transfer running: ${JSON.stringify(seen)}`);
  }
  const stopped = seen.stoppedMs ?? Number.POSITIVE_INFINITY;
  if (stopped > refusedAfterMs + 1_000) {
    throw new Error(
      `ended the transfer ${stopped}ms into the GET, whose refusal came at ${
        refusedAfterMs
      }ms: ${JSON.stringify(seen)}`,
    );
  }
  if (seen.finished) {
    throw new Error(`the whole body went out: ${JSON.stringify(seen)}`);
  }
  return seen;
};

Deno.test("an in-process fetch ends the transfer of a report whose length was declared", () =>
  withFake(async (s3) => {
    const restore = useFake(s3);
    try {
      // 10 MB with its length declared, the first byte held back 250 ms and a
      // chunk every 25 ms after: nothing to refuse from but the length, and
      // some sixteen seconds of sending left running if the refusal does not
      // end it.
      s3.stream(BUCKET, report, {
        total: 10 * 1024 * 1024,
        chunkBytes: 16_384,
        firstDelayMs: 250,
        chunkDelayMs: 25,
      });
      const { ms, message } = await refuse(report);
      if (message !== `${report} is larger than ${MAX_BYTES} bytes`) {
        throw new Error(`refused with: ${message}`);
      }
      const seen = await assertEnded(s3, ms);
      // Refused from the header alone: the body was never taken up, so there
      // was nothing to give up on either.
      if (seen.sent !== 0) {
        throw new Error(`read ${seen.sent} bytes of the body: ${JSON.stringify(seen)}`);
      }
    } finally {
      restore();
    }
  }));

Deno.test("an in-process fetch ends the transfer of a report with no declared length", () =>
  withFake(async (s3) => {
    const restore = useFake(s3);
    try {
      // The same offer with no Content-Length, so the cap is reached only by
      // reading: the refusal comes partway through a body that is arriving,
      // and the reading has to be given up on as well as stopped.
      s3.stream(BUCKET, report, {
        total: 10 * 1024 * 1024,
        chunkBytes: 16_384,
        chunkDelayMs: 25,
        contentLength: false,
      });
      const { ms, message } = await refuse(report);
      if (message !== `${report} is larger than ${MAX_BYTES} bytes`) {
        throw new Error(`refused with: ${message}`);
      }
      await assertEnded(s3, ms);
    } finally {
      restore();
    }
  }));
