// A small stand-in for S3, enough for the three calls this example makes:
// GetObject, PutObject with If-None-Match, and HeadObject with a checksum.
// It lets the scripts be tested with no AWS account and no network.

interface StoredObject {
  body: Uint8Array;
  etag: string;
  checksumSha256: string;
}

const base64 = (bytes: Uint8Array) => btoa(String.fromCharCode(...bytes));

// The byte every streamed body sends, so that one filler byte means the same
// in the body and in the checksum sent with it.
const FILLER = 0x78;

const CRC32 = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  return table;
})();

/**
 * The CRC32 of `length` filler bytes, base64 as S3 sends it. The checksum
 * travels in the response headers, before the body it covers, so it is worked
 * out from the size the stream was asked for rather than from what went out:
 * a client that read such a body to the end finds this to match.
 */
const crc32OfFiller = (length: number): string => {
  let crc = 0xffffffff;
  for (let at = 0; at < length; at++) {
    crc = CRC32[(crc ^ FILLER) & 0xff] ^ (crc >>> 8);
  }
  crc = (crc ^ 0xffffffff) >>> 0;
  return base64(
    new Uint8Array([
      (crc >>> 24) & 0xff,
      (crc >>> 16) & 0xff,
      (crc >>> 8) & 0xff,
      crc & 0xff,
    ]),
  );
};

async function digest(body: Uint8Array) {
  const hash = new Uint8Array(
    await crypto.subtle.digest("SHA-256", new Uint8Array(body)),
  );
  const hexHash = Array.from(hash, (b) => b.toString(16).padStart(2, "0")).join(
    "",
  );
  return { checksumSha256: base64(hash), etag: `"${hexHash.slice(0, 32)}"` };
}

function error(status: number, code: string, message: string): Response {
  return new Response(
    `<?xml version="1.0" encoding="UTF-8"?><Error><Code>${code}</Code><Message>${message}</Message></Error>`,
    { status, headers: { "content-type": "application/xml" } },
  );
}

/**
 * A GET served from a stream rather than from stored bytes, so a test can
 * watch what a client does with a body that is too big: how much of it was
 * written, and whether the client went away before the end.
 */
export interface StreamBody {
  /** Bytes the server tries to send. */
  total: number;
  /** Bytes per chunk. */
  chunkBytes?: number;
  /** Pause before the first chunk, so a step that refuses from the declared
   * length has time to give up before a byte of the body is on its way. */
  firstDelayMs?: number;
  /** Pause before every chunk after the first, so a step that refuses
   * partway through is not racing the socket's own buffers. Names on their
   * own pause every chunk, the first included. */
  chunkDelayMs?: number;
  /** False leaves off Content-Length, so only counting the bytes as they
   * arrive can tell how big the body is. */
  contentLength?: boolean;
  /** Send the body's CRC32 as an x-amz-checksum-crc32 header, as S3 answers a
   * whole-object GET once checksum mode is asked for, which the SDK asks for
   * by default. The SDK then hands the step its own checksum stream instead
   * of the connection's body, and that stream is what a refusal has to give
   * up on. */
  checksum?: boolean;
}

/** What the server saw of one streamed GET. */
export interface Transfer {
  /** Bytes handed over before the GET ended. A chunk still waiting out its
   * delay when the client gave up is not counted, because it never went. */
  sent: number;
  /** The whole body went out. */
  finished: boolean;
  /** The response stream was given up on. Deno ends a stream this way when
   * a client stops reading, and when it closes after the last byte, so a
   * test reads this with finished, which says whether bytes were left. */
  closedEarly: boolean;
  /** Milliseconds between this GET beginning and the stream being given up
   * on; missing while the server is still sending. This is the server's
   * clock, started when its handler ran, so a test that has its own start
   * times the close from there and quotes this beside it. */
  stoppedMs?: number;
}

export class FakeS3 {
  readonly objects = new Map<string, StoredObject>();
  readonly requests: string[] = [];
  readonly transfers = new Map<string, Transfer>();
  #streams = new Map<string, StreamBody>();
  #server: Deno.HttpServer<Deno.NetAddr>;

  constructor() {
    this.#server = Deno.serve(
      { hostname: "127.0.0.1", port: 0, onListen() {} },
      (request) => this.#handle(request),
    );
  }

  get endpoint(): string {
    return `http://127.0.0.1:${this.#server.addr.port}`;
  }

  async put(bucket: string, key: string, text: string): Promise<StoredObject> {
    const body = new TextEncoder().encode(text);
    const stored = { body, ...(await digest(body)) };
    this.objects.set(`${bucket}/${key}`, stored);
    return stored;
  }

  /** Serves this key's GET as a stream; see StreamBody for how. */
  stream(bucket: string, key: string, body: StreamBody): void {
    const path = `${bucket}/${key}`;
    this.#streams.set(path, body);
    this.transfers.set(path, {
      sent: 0,
      finished: false,
      closedEarly: false,
      stoppedMs: undefined,
    });
  }

  /**
   * Ends the server.
   *
   * Deno's shutdown waits for a connection whose client has stopped reading,
   * and on a refusal that client is the step under test. A step that gave up
   * on an over-big body has closed its connection on the way out, so the
   * shutdown goes at once. A step that left a transfer running is the thing
   * these tests look for, and waiting behind it would hang the test instead of
   * reporting it — nothing the server does can finish a response nobody is
   * reading — so the wait is bounded. What that step left behind is on the
   * transfer record, which is where the test that asked for it reads it.
   */
  async close(): Promise<void> {
    const shuttingDown = this.#server.shutdown();
    let patience: ReturnType<typeof setTimeout> | undefined;
    const ended = await Promise.race([
      shuttingDown.then(() => true),
      new Promise<boolean>((ready) => {
        patience = setTimeout(() => ready(false), 2_000);
      }),
    ]);
    if (patience !== undefined) clearTimeout(patience);
    if (ended) {
      await shuttingDown;
      return;
    }
    // Nobody is waiting on this shutdown any more. If it fails, it fails on a
    // connection no test is watching any more either.
    shuttingDown.catch(() => {});
    this.#server.unref();
  }

  /** The streamed body, and a record of how far the client let it go. */
  #streamed(path: string, spec: StreamBody): Response {
    const transfer = this.transfers.get(path)!;
    const began = performance.now();
    const chunkBytes = spec.chunkBytes ?? 8192;
    const headers: Record<string, string> = {
      etag: '"streamed"',
      "content-type": "text/markdown; charset=utf-8",
    };
    if (spec.contentLength !== false) headers["content-length"] = String(spec.total);
    if (spec.checksum) headers["x-amz-checksum-crc32"] = crc32OfFiller(spec.total);
    let written = 0;
    let pulls = 0;
    // Set when the client gives up, so a pull that was waiting out its delay
    // sends nothing more and `sent` stays what went out before the end.
    let cancelled = false;
    return new Response(
      new ReadableStream<Uint8Array>({
        async pull(controller) {
          // firstDelayMs holds the first byte back, so a refusal from the
          // declared length lands before any of the body is on its way;
          // chunkDelayMs paces every later chunk, so a refusal partway
          // through does not race the socket's own buffers.
          const delay = pulls++ === 0
            ? spec.firstDelayMs ?? spec.chunkDelayMs ?? 0
            : spec.chunkDelayMs ?? 0;
          if (delay) await new Promise((r) => setTimeout(r, delay));
          if (cancelled) return;
          if (written >= spec.total) {
            transfer.finished = true;
            controller.close();
            return;
          }
          const size = Math.min(chunkBytes, spec.total - written);
          written += size;
          // Ordinary text bytes, so a size test is not also a decoding test.
          transfer.sent += size;
          controller.enqueue(new Uint8Array(size).fill(FILLER));
        },
        cancel() {
          cancelled = true;
          // Deno ends a response this way whether the client gave up or the
          // body went out whole, so only a stream with bytes left counts, and
          // only the first such ending is timed.
          if (!transfer.finished && transfer.stoppedMs === undefined) {
            transfer.closedEarly = true;
            transfer.stoppedMs = Math.round(performance.now() - began);
          }
        },
      }),
      { headers },
    );
  }

  async #handle(request: Request): Promise<Response> {
    const path = decodeURIComponent(new URL(request.url).pathname.slice(1)); // <bucket>/<key>
    this.requests.push(`${request.method} ${path}`);
    const existing = this.objects.get(path);

    if (request.method === "PUT") {
      if (request.headers.get("if-none-match") === "*" && existing) {
        return error(
          412,
          "PreconditionFailed",
          "At least one of the pre-conditions you specified did not hold",
        );
      }
      const body = new Uint8Array(await request.arrayBuffer());
      const stored = { body, ...(await digest(body)) };
      const claimed = request.headers.get("x-amz-checksum-sha256");
      if (claimed && claimed !== stored.checksumSha256) {
        return error(
          400,
          "BadDigest",
          "The checksum you specified did not match what we received",
        );
      }
      this.objects.set(path, stored);
      return new Response(null, {
        headers: {
          etag: stored.etag,
          "x-amz-checksum-sha256": stored.checksumSha256,
        },
      });
    }

    const streamed = request.method === "GET" ? this.#streams.get(path) : undefined;
    if (streamed) return this.#streamed(path, streamed);
    if (!existing) {
      return error(404, "NoSuchKey", "The specified key does not exist.");
    }
    const headers: Record<string, string> = {
      etag: existing.etag,
      "content-length": String(existing.body.length),
      "content-type": "text/markdown; charset=utf-8",
    };
    if (request.headers.get("x-amz-checksum-mode") === "ENABLED") {
      headers["x-amz-checksum-sha256"] = existing.checksumSha256;
    }
    return new Response(
      request.method === "HEAD" ? null : new Uint8Array(existing.body),
      { headers },
    );
  }
}
