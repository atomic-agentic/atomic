// Writes one text object to the reports bucket, at most once.
// stdin:  {"key": "digests/2026-10.md", "text": "..."}
// stdout: {"key", "sha256", "etag", "created"}
//
// A step can run more than once: a crash after S3 accepted the write but
// before Atomic saved the result makes the step run again. This script is
// safe to repeat. It creates the object only if the key is free. When the key
// is taken, it checks whether the object already holds exactly this content.
// If so, an earlier attempt did the work. If not, it stops and overwrites
// nothing.
import {
  HeadObjectCommand,
  PutObjectCommand,
  S3Client,
} from "npm:@aws-sdk/client-s3@3.1169.0";

function required(name: string): string {
  const value = Deno.env.get(name);
  if (!value) throw new Error(`${name} is not set`);
  return value;
}

// The bucket is deployment configuration. A caller never chooses it.
const bucket = required("REPORTS_BUCKET");

// Region and credentials are passed explicitly, so the SDK does not go looking
// for a profile, a config file or an instance role.
const s3 = new S3Client({
  region: required("AWS_REGION"),
  credentials: {
    accessKeyId: required("AWS_ACCESS_KEY_ID"),
    secretAccessKey: required("AWS_SECRET_ACCESS_KEY"),
    sessionToken: Deno.env.get("AWS_SESSION_TOKEN"),
  },
});

const input: { key: string; text: string } = JSON.parse(
  await new Response(Deno.stdin.readable).text(),
);
const body = new TextEncoder().encode(input.text);
const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", body));
const checksum = btoa(String.fromCharCode(...digest));
const sha256 = Array.from(digest, (b) => b.toString(16).padStart(2, "0")).join(
  "",
);

try {
  const created = await s3.send(
    new PutObjectCommand({
      Bucket: bucket,
      Key: input.key,
      Body: body,
      ContentType: "text/markdown; charset=utf-8",
      ChecksumSHA256: checksum,
      IfNoneMatch: "*", // create only; never replace
    }),
  );
  console.log(
    JSON.stringify({
      key: input.key,
      sha256,
      etag: created.ETag ?? "",
      created: true,
    }),
  );
} catch (error) {
  const status = (error as { $metadata?: { httpStatusCode?: number } })
    .$metadata?.httpStatusCode;
  if (status !== 412) throw error;
  // The key is taken. Did an earlier attempt of this step write it?
  const existing = await s3.send(
    new HeadObjectCommand({
      Bucket: bucket,
      Key: input.key,
      ChecksumMode: "ENABLED",
    }),
  );
  if (existing.ChecksumSHA256 !== checksum) {
    throw new Error(
      `${input.key} already exists with different content; nothing was written`,
    );
  }
  console.log(
    JSON.stringify({
      key: input.key,
      sha256,
      etag: existing.ETag ?? "",
      created: false,
    }),
  );
}
