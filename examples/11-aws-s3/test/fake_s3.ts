// A small stand-in for S3, enough for the three calls this example makes:
// GetObject, PutObject with If-None-Match, and HeadObject with a checksum.
// It lets the scripts be tested with no AWS account and no network.

interface StoredObject {
  body: Uint8Array;
  etag: string;
  checksumSha256: string;
}

const base64 = (bytes: Uint8Array) => btoa(String.fromCharCode(...bytes));

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
  /** Pause before each chunk, so an early stop is observed, not raced. */
  delayMs?: number;
  /** False leaves off Content-Length, so only counting the bytes as they
   * arrive can tell how big the body is. */
  contentLength?: boolean;
}

/** What the server saw of one streamed GET. */
export interface Transfer {
  /** Bytes written before the GET ended. */
  sent: number;
  /** The whole body went out. */
  finished: boolean;
  /** The response stream was given up on. Deno ends a stream this way when
   * a client stops reading, and when it closes after the last byte, so a
   * test reads this with finished, which says whether bytes were left. */
  closedEarly: boolean;
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
    this.transfers.set(path, { sent: 0, finished: false, closedEarly: false });
  }

  close(): Promise<void> {
    return this.#server.shutdown();
  }

  /** The streamed body, and a record of how far the client let it go. */
  #streamed(path: string, spec: StreamBody): Response {
    const transfer = this.transfers.get(path)!;
    const chunkBytes = spec.chunkBytes ?? 8192;
    const headers: Record<string, string> = {
      etag: '"streamed"',
      "content-type": "text/markdown; charset=utf-8",
    };
    if (spec.contentLength !== false) headers["content-length"] = String(spec.total);
    let written = 0;
    return new Response(
      new ReadableStream<Uint8Array>({
        async pull(controller) {
          if (spec.delayMs) await new Promise((r) => setTimeout(r, spec.delayMs));
          if (written >= spec.total) {
            transfer.finished = true;
            controller.close();
            return;
          }
          const size = Math.min(chunkBytes, spec.total - written);
          written += size;
          // Ordinary text bytes, so a size test is not also a decoding test.
          transfer.sent += size;
          controller.enqueue(new Uint8Array(size).fill(0x78));
        },
        cancel() {
          // Reached only when the reader went away before the body ended.
          if (!transfer.finished) transfer.closedEarly = true;
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
