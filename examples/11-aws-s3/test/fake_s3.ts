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

export class FakeS3 {
  readonly objects = new Map<string, StoredObject>();
  readonly requests: string[] = [];
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

  close(): Promise<void> {
    return this.#server.shutdown();
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
