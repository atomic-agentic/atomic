import { GetObjectCommand, S3Client } from "npm:@aws-sdk/client-s3@3.1169.0";
interface SummarizeReportInput { key: string }



const MAX_BYTES = 200_000;

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
    const bytes = await object.Body!.transformToByteArray();
    if (bytes.length > MAX_BYTES) {
      throw new Error(`${input.key} is larger than ${MAX_BYTES} bytes`);
    }
    const hash = await crypto.subtle.digest("SHA-256", new Uint8Array(bytes));
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
