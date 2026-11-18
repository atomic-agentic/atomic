# Models

An atom names a model tier or a model; the project maps tiers to models, and
the endpoint is any OpenAI-compatible API: OpenAI's own, or a local server such
as llama.cpp, vLLM, Ollama or Lemonade.

## Tiers

From least to most capable:

| Tier | For |
| --- | --- |
| `basic` | the smallest, cheapest models, often local: classify, extract, route |
| `fast` | quick, low-cost general models: executors and routine steps |
| `advanced` | capable general models: reviewers and harder steps |
| `frontier` | the most capable models available: planning and the reviews that decide the most |

A tier says which models run an atom; an atom's kind says how it runs. The two
are independent: a lite atom may run on any tier.

## How the output schema reaches the model

Every atom's output contract is lowered to the strict generation profile and
sent with the request, so a provider that supports structured output can hold
the model to it while it decodes. What each kind of atom sends, and who
enforces it:

| Kind | API | Schema sent as | Enforced by |
| --- | --- | --- | --- |
| lite | Chat Completions (any endpoint but OpenAI's API) | `response_format: json_schema` | llama.cpp, vLLM and Ollama grammars; most OpenAI-compatible servers |
| lite | Responses (OpenAI's API) | `text.format: json_schema` | OpenAI |
| codex | Responses (Codex speaks no other) | `text.format: json_schema` | OpenAI; some other servers accept it and ignore it |

So a Codex atom on a local server may be generating free text. For that case,
a Codex atom on any endpoint other than OpenAI's API also gets the lowered
schema written into its developer instructions. Whatever the model writes is
then checked the same way: an answer that is exactly one fenced code block is
read as its contents, the result is validated against the full contract, and a
misfit gets one repair turn that quotes what was wrong.

Measured with `cargo run -p atomic-agentic-atoms --example schema_fit` on
Qwen3.6-35B-A3B through Lemonade's llama.cpp backend, which ignores the
Responses schema, over the `draft-task` contract as a Codex atom:

| Schema in the instructions | Runs | First turn fits | After the repair turn | Failed |
| --- | --- | --- | --- | --- |
| no | 10 | 0 | 1 | 9 |
| yes | 20 | 5 | 15 | 0 |

When a task needs no shell, files or tools, a lite atom is the better fit on
such a server: its schema is enforced in decoding, and it makes one call.

Run the example against your own endpoint to see how a model holds up:

```bash
ATOMIC_MODEL_BASE_URL=http://127.0.0.1:13305/api/v1 ATOMIC_MODEL=<model> \
    cargo run -p atomic-agentic-atoms --example schema_fit -- 3
```
