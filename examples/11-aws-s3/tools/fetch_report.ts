import { GetObjectCommand, S3Client } from "npm:@aws-sdk/client-s3@3.1169.0";
interface SummarizeReportInput { key: string }



// The most report text this step will hold. Exported so that its tests name
// the same number the code enforces.
export const MAX_BYTES = 200_000;

// A standalone workflow step fetches this report before the model call.
// The workflow checkpoints its output; script code creates no nested steps.
export async function fetchReport(input: SummarizeReportInput) {
    const s3 = new S3Client({
      region: Deno.env.get("AWS_REGION"),
      credentials: {
        accessKeyId: Deno.env.get("AWS_ACCESS_KEY_ID")!,
        secretAccessKey: Deno.env.get("AWS_SECRET_ACCESS_KEY")!,
        sessionToken: Deno.env.get("AWS_SESSION_TOKEN"),
      },
    });
    const object = await s3.send(
      new GetObjectCommand({
        Bucket: Deno.env.get("REPORTS_BUCKET")!,
        Key: input.key,
      }),
    );
    // When S3 declares the length, an object that is already too big is
    // refused before its body is read.
    const length = object.ContentLength;
    if (length !== undefined && length > MAX_BYTES) {
      throw new Error(`${input.key} is larger than ${MAX_BYTES} bytes`);
    }
    // Counts the body as it arrives, so an object whose length was not
    // declared is refused partway through instead of after all of it.
    const chunks: Uint8Array[] = [];
    const reader = object.Body!.transformToWebStream().getReader();
    let total = 0;
    try {
      for (;;) {
        const chunk = await reader.read();
        if (chunk.done) {
          break;
        }
        total += chunk.value.length;
        if (total > MAX_BYTES) {
          // Stops here; what was read is dropped with this call frame.
          await reader.cancel();
          throw new Error(`${input.key} is larger than ${MAX_BYTES} bytes`);
        }
        chunks.push(chunk.value);
      }
    } finally {
      reader.releaseLock();
    }
    // One copy, bounded by MAX_BYTES however the body was cut up.
    const bytes = new Uint8Array(total);
    let at = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, at);
      at += chunk.length;
    }
    const hash = await crypto.subtle.digest("SHA-256", bytes);
    const report = {
      text: new TextDecoder("utf-8", { fatal: true }).decode(bytes),
      bytes: bytes.length,
      // Says exactly which content the model was shown.
      sha256: Array.from(new Uint8Array(hash), (b) => b.toString(16).padStart(2, "0")).join(""),
    };
  return { report };
}
if (import.meta.main) {
  const input = JSON.parse(await new Response(Deno.stdin.readable).text());
  console.log(JSON.stringify(await fetchReport(input)));
}
