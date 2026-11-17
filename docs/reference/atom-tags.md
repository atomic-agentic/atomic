# Atom body tags

An atom's body (`.atom.md`) is Markdown with a fixed set of `{% … %}` tags.
Tags are found on the raw text before Markdown is parsed, so nothing inside a
tag is ever read as Markdown: `{% $input.file_path %}` keeps its underscore.
Inside a fenced code block (```` ``` ```` or `~~~`), `{%` is plain text. Any
other tag fails the load with its line and column.

## Grammar

```text
tag        = "{%" ws* body ws* "%}"            (on one line)
body       = value | if | else | end | partial | example
value      = "$" name ( "." name | "[" digits "]" )*
if         = "if" ws+ condition                 condition: CEL, up to "%}"
else       = "else" ws* "/"
end        = "/" ( "if" | "example" )
partial    = "partial" ws+ 'file="' path '"' ws* "/"
example    = "example" ws+ 'name="' text '"'
name       = ( letter | digit | "_" )+          not starting with a digit
```

A `"…"` or `'…'` string inside a condition may contain `%}`.

## Tags

| Tag | Purpose |
| --- | --- |
| `{% $input.path %}`, `{% $frontmatter.path %}` | Insert a value into the user turn |
| `{% if <CEL> %}…{% else /%}…{% /if %}` | Conditional instructions; `else` is optional |
| `{% partial file="_name.md" /%}` | Include a shared fragment |
| `{% example name="…" %}…{% /example %}` | A few-shot example, with a `json input` and a `json output` fenced block |

`if` and `example` blocks must balance; an `example` cannot contain another
example, and `else` belongs to the nearest open `if`.

These tags are part of the grammar but not supported by this version yet, and
fail the load with a message saying so: `for`, `tool`, `schema`, `render`,
`file`, `slot`, and function calls such as `{% truncate($input.x, 80) %}`.
