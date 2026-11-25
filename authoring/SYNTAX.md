# Astro expression contract

Atomic uses Astro syntax and its real parser, with a deliberately bounded
TypeScript-shaped interpreter. Atom and component modules never execute; named
imports of local TypeScript helpers run in the host's embedded runtime, one
fresh isolate per call (see Helpers below). TypeScript checks
against fresh schema-derived declarations before a model call. Suppression
comments and explicit `any` in production frontmatter cannot disable that gate.
Diagnostics use Astro's source map to point back to the authored file.

## Values and operators

Expressions read input, verified public context, component props and callback
locals. Supported values are JSON primitives, arrays and plain objects. Missing
interpolated data is an error. Input/case/judge data must be plain JSON, with no
accessors or runtime objects.

Property reads, strict equality/inequality, comparisons, `!`, unary minus,
`+`, `-`, `*`, `/`, `&&`, `||`, `??` and ternaries use JavaScript-shaped semantics.
Logical operators short-circuit. As in JSX, `{flag && <X />}` renders nothing when
`flag` is `false`, `''`, `null` or missing, and renders `0` as data. Negative
number literals are allowed in configuration and component defaults. Array methods `map`, `filter`, `some`, `every`
accept one expression-bodied callback; predicates must return booleans. Object
construction and plain object destructuring are supported. Component defaults
apply to undefined, preserving explicit null. Strings use JavaScript UTF-16
length; full JSON Schema string constraints remain independently enforced.

Numbers are finite IEEE-754 values. Integer literals, data and computed results
must remain within the safe integer range. Non-finite arithmetic, including
division by zero, fails explicitly. This rejects cases ordinary JavaScript would
allow or round; no silent large-integer conversion crosses the Rust worker boundary.

A leaf optional read such as `input.extra?.note` is supported, commonly with `??`.
Continued chains (`input.extra?.note.length`) and optional collection calls are
rejected. Use explicit conditions or nullish fallback. The interpreter does not
claim to implement all JavaScript chain/coercion behavior. Direct missing values
fail rather than silently disappearing from a prompt.

## Components and contracts

Import only local default `.astro` components at runtime. Type-only imports
supply generated declarations. The fixed authoring intrinsics have explicit
implementations, such as `fitsWithin`. Components must remain under the project
root, have no import cycles, and use supported props, literal sections,
fragments and slots.

Frontmatter may declare `const` locals bound to supported expressions, with
plain or nested object destructuring; they evaluate in order when the component
renders. Template literals interpolate strings, numbers, booleans and null and
carry the least trusted origin of their parts. Named slots (`<slot name="x">`
with fallback content, filled by a direct child with `slot="x"`) follow Astro:
explicit content, even an empty Fragment, is present; whitespace and comments
are not; and a forwarded slot is present only when the slot it forwards is.
`Astro.slots.has('x')` reads presence without rendering.

## Helpers

A named import of a local `.ts` module, written without the extension
(`import { label } from '../lib/review'`), imports helpers. The compiler
captures the helper graph's runtime imports, type-checks it with the atom and
records it with the compiled atom; type-only imports are never captured. Each
call runs in a fresh Boa runtime with ECMAScript built-ins only: no network,
files, environment, timers, WebAssembly or shared memory. Arguments and
results are JSON. A result is as trusted as its least trusted argument; one
computed from literals alone is authored and may be instructions. Date,
Temporal.Now and Math.random follow the step's recorded clock and seed, and
local time is UTC with an en-US default locale. Limits: 1 s per call, 128 MiB
heap, 64 MiB of ArrayBuffers, 1 MiB result, 16 KiB error text, and per operation
256 calls, 10 s and 4 MiB of results. A helper failure fails the operation; in a
validator it is an error, not a rejection.

Frontmatter permits literal `config`, prop destructuring and type declarations.
Executable statements, arbitrary calls/globals, assignment, spreads, directives,
async or block callbacks, scripts, styles and reserved data wrapper elements fail.
Quoted CEL validators and scorer expressions are rejected. Workflow YAML mappings
and existing eval assertion strings continue to use CEL.

Validators return actual booleans. A validator that returns false, or fails on
the answer (reading a missing field, too large a collection), rejects the answer
and the rejection quotes the rule's source for the repair turn. The answer has
its own size budget, separate from the input's. Judge bindings validate input, candidate output,
case metadata and their constructed result against complete schemas. Judge outputs
are schema-validated before scoring. Scores must be finite and in [0,1]; aggregate
scores require a nonempty list and enforce the configured mean threshold. Calling
a judge model is separate workflow work, not an interpreter callback.

## Resource and trust boundaries

Each source is limited to 262,144 UTF-16 units; the transitive graph to 1,048,576.
Component depth is bounded at 32 and the graph at 128 components. Render/contract
operations are limited to 10,000, collection operations to 1,000 elements and
rendered prompt size to 1,048,576 UTF-16 units. Plain contract JSON has depth/node
limits. The Rust bridge bounds request/response bytes and gives a worker 30 seconds.

Static text and selected literal branches remain developer instructions. Computed
or interpolated data carries provenance into tagged user bindings, including data
inside a mapped list, a filtered list or an object literal passed as a prop. A
binding's wrapper name depends on every bound value, so a payload cannot forge
another binding's wrapper. Payloads cannot
close their own wrappers, introduce components or execute expressions. Sections and
slots preserve this distinction. Formatting changes text and is opt-in.

The compiled fingerprint covers source/component bytes, complete resolved schemas,
generated declarations and the native frontend and bundled worker. Full-schema constraints that
TypeScript erases still change the fingerprint. Offline resolution preserves schema
identifier scopes, anchors, file retrieval aliases and reference sibling constraints.
Recursive/dynamic schemas that cannot be represented are rejected explicitly. Direct context checkpoints refuse a different compiled
version, validate integrity and run identity, and restore without live settings/env
reads. The snapshot's checksum detects corruption; the checkpoint's signature,
keyed apart from the state directory, detects changes by anything that cannot read
the user's files, not by a process running as that user. Workflow-level persistence and migrations remain separate work.

Full input/output/judge JSON Schema validation runs in Rust independently of the
TypeScript projection. The public context resolver checks the shape keywords
(`type`, `properties`, `required`, `additionalProperties`, `items`, `enum`, `const`,
local `$ref`) and leaves constraint keywords to the host's full-schema validation.
Keywords that change a value's shape, such as `anyOf`, are refused, never ignored. Secret credentials do not enter prompt-visible context.
