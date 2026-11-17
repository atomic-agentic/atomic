---
atomic: 1
description: Store one digest in the reports bucket in S3, without replacing an existing object.
kind: exec
exec:
  script: ../tools/put_object.ts
env:
  AWS_REGION: us-east-1
  REPORTS_BUCKET: acme-reports
credentials: [aws-digest-writer]
runtime:
  deno: { sys: [osRelease] }
network:
  allow: [acme-reports.s3.us-east-1.amazonaws.com]
effects:
  external: idempotent
---
