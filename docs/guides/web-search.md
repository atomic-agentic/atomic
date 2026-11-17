# Web search

Codex includes a native **OpenAI-hosted search tool**. Atomic plans to expose it
in Codex-backed sessions and atoms; it does not require a separate search service
or Atomic's later retrieval/RAG subsystem. Atomic integration remains planned.

For ordinary coding, ask a focused question:

> Find the current public documentation for this library's migration API. Compare
> it with our usage and cite the relevant pages before proposing a change.

Search runs remotely, including cached search. It can be available when shell
commands have no network access. Keep private code, customer data and Slack text
out of public queries unless your data policy explicitly permits sending it.

## Choose a search mode

| Mode | Upstream behavior |
| --- | --- |
| `cached` | Use the OpenAI-maintained search cache; the normal local-chat default |
| `indexed` | Permit external retrieval with search-index gating |
| `live` | Fetch current web information |
| `disabled` | Omit the native search tool |

Full-access sessions can have a different default. See
[OpenAI's search configuration](https://learn.chatgpt.com/docs/config-file/config-basic#web-search-mode).
Cached search is not an offline copy of the web. Domain/search restrictions are
separate from shell network rules.

In upstream Codex's `config.toml`:

```toml
web_search = "live"
```

Atomic's proposed atom frontmatter passthrough is:

```yaml
codex:
  web_search: live
```

This is a fragment, not a complete atom. See the
[`codex` field](../reference/atom-files.md#fields). Managed policy constrains the
selection. Durable-atom search defaults remain unspecified; inspect the resolved
setting before relying on remote retrieval.

## Who provides the search engine?

**OpenAI provides the native hosted search capability.** The public Codex client
and documentation do not identify a guaranteed underlying engine such as Bing or
Google, or offer an engine selector for the standard hosted tool.

OpenAI separately discloses that ChatGPT Enterprise/Edu search may send
disassociated queries to Bing. That disclosure describes those ChatGPT products;
it does not establish the backend for every Codex request.
[OpenAI's disclosure](https://help.openai.com/en/articles/10093903-chatgpt-search-for-enterprise-and-edu#information-shared-when-chatgpt-searches).

## Use search in a durable workflow

Have a research atom return a typed answer and source URLs, then save that result
before proposing a change. Include retrieval time and a content hash or artifact
when exact evidence matters: a URL alone is not a stable page version.

Recovery reuses a completed research step. An interrupted atom may search again
and see different information. Retrieved pages are external data; instructions
inside them do not authorize tools or change workflow policy. Check citations and
whether the conclusions follow from the saved evidence.

## Other search paths

Native search is separate from an MCP search tool, a browser, shell HTTP, Slack
search or retrieval over your own corpus. Each has its own credentials, egress
policy and data boundary. A lite atom or local endpoint does not automatically
provide Codex's hosted tool. See [tools and integrations](tools-and-integrations.md)
and [security and permissions](security-and-permissions.md).

For an independently selected backend, use an approved MCP or typed search tool
with its own result contract. Disable native search when policy requires all
queries to use that alternative; adding a tool does not redirect the native one.

## Underlying implementation

This section describes the upstream client and hosted protocol for readers who
need to understand the integration boundary. The linked source is a fixed
November 16, 2026 snapshot; Atomic's eventual pinned revision may differ.

### Client declaration and hosted execution

1. Codex builds a `ToolSpec::WebSearch` declaration for the model request, with
   mode, filters and optional location/context settings.
2. The hosted service performs retrieval during the model's response. Supported
   model/endpoint combinations can search, open pages and find text within them;
   those actions are not shell commands in the local workspace.
3. The Responses API represents search activity as `web_search_call` items and
   citations as URL annotations. Codex exposes search activity in its transcript
   and JSON event output.

The [hosted-tool builder](https://github.com/openai/codex/blob/237d8b104ed06ae46c5ed9a54693672dea21856c/codex-rs/core/src/tools/hosted_spec.rs)
shows the local declaration. The
[Responses web-search guide](https://developers.openai.com/api/docs/guides/tools-web-search#output-and-citations)
describes hosted actions and citation output; capabilities vary by endpoint/model.

At this snapshot, `cached` maps to `external_web_access: false`, `live` to
`external_web_access: true`, and `indexed` adds `indexed_web_access: true` with
external access enabled. `disabled` omits the tool. These flags govern hosted
retrieval, not command-network access in the sandbox.

### What is included in the open-source code?

| Component | Location |
| --- | --- |
| Tool declaration, modes, filters and display of activity | Codex client and its provider protocol |
| Search index, retrieval and search execution | Remote service, outside the executable |
| Checkpointed research output and workflow policy | Atomic's planned integration |

Embedding the Codex crates does not install a search index or a self-hosted search
backend. Offline deployments disable remote search or use an approved alternative.
Hosted search bypasses the command-network proxy and its domain allowlist; it has
separate settings and managed restrictions.
[Upstream search behavior](https://learn.chatgpt.com/docs/web-search#configure-local-web-search).

### Experimental custom-provider search

Upstream documents `supports_standalone_web_search` for custom providers. It
requires a compatible endpoint, model and runtime; it defaults off and remains
under development. The flag alone does not add web search to an arbitrary local
model. See [custom-provider search](https://learn.chatgpt.com/docs/web-search#search-with-a-custom-model-provider).

Next: [tools and integrations](tools-and-integrations.md), [Slack](slack.md), or
[durable execution](../concepts/durable-execution.md).
