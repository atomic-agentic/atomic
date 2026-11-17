# SPDX-License-Identifier: Apache-2.0
"""Offline model of the two proposed atom template syntaxes, for the example checker.

This is not an implementation of atomic. It turns an `.atom.md` body (Markdoc-style
`{% ... %}` tags) or an `.atom.mdx` body (JSX-style tags) into one list of operations,
so the checker can validate tags against schemas and confirm that two forms of the same
atom say the same thing. It covers only the tags the catalog uses.

Operations are tuples:
    ("text", str)                 literal prompt text
    ("val", path)                 an inserted value, e.g. "input.item.title" or "c"
    ("if", cel) ("else",) ("endif",)
    ("for", var, path) ("endfor",)
    ("partial", file, params)     self-closing include
    ("partial_open", file, params) ("partial_close",)
    ("render", path)              render a value with the partial bound to its schema
    ("slot",)                     where a partial places the content it was given
    ("example", name, {"input": json_text, "output": json_text})
"""

from __future__ import annotations

import re

PATH = r"[A-Za-z_]\w*(?:\.\w+)*"
JSON_BLOCKS = re.compile(r"```json (input|output|prepared)\n(.*?)\n```", re.S)


def split_frontmatter(text):
    """Returns (frontmatter_text or None, body). Frontmatter is optional for partials."""
    match = re.fullmatch(r"---\n(.*?)\n---\n(.*)", text, re.S)
    return (match[1], match[2]) if match else (None, text)


def _fail(where, message):
    raise ValueError(f"{where}: {message}")


# --- Markdoc-style tags: .atom.md ---------------------------------------------------

MD_TAG = re.compile(r"\{%\s*(.*?)\s*%\}", re.S)


def _md_attrs(text, where):
    attrs = {}
    for key, value in re.findall(r'(\w+)=("[^"]*"|[^\s"]+)', text):
        if key in attrs:
            _fail(where, f"attribute {key} is repeated")
        attrs[key] = value
    return attrs


RESERVED_TAG_PREFIX = "data-"  # atomic wraps inserted values in <data-...> elements
SECTION_LINE = re.compile(r"\s{0,3}<(/?)([a-z][\w.-]*)(?:\s[^<>]*)?(/?)>\s*")


def md_sections(body, where):
    """XML-style section tags in `.atom.md`.

    A lowercase tag alone on its line is a section and must be balanced. An angle
    bracket inside a line of prose is plain text and is not checked, so generics
    and placeholders need no escaping. The reserved wrapper prefix is rejected
    wherever it appears outside code.
    """
    open_sections = []
    in_fence = False
    lines = body.split("\n")
    for number, line in enumerate(lines, 1):
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        prose = re.sub(r"`[^`\n]*`", "", line)
        if re.search(r"</?" + RESERVED_TAG_PREFIX, prose):
            _fail(where, f"line {number}: tags starting with {RESERVED_TAG_PREFIX!r} are reserved for inserted values")
        match = SECTION_LINE.fullmatch(line)
        if not match or match[3]:
            continue
        closing, name = match[1], match[2]
        if closing:
            if not open_sections or open_sections.pop() != name:
                _fail(where, f"line {number}: </{name}> does not close an open section")
        else:
            open_sections.append(name)
            following = lines[number] if number < len(lines) else ""
            if following.strip():
                _fail(where, f"line {number}: leave a blank line after <{name}> so Markdown inside the section is parsed")
    if open_sections:
        _fail(where, f"section <{open_sections[-1]}> is never closed")


def md_ops(body, where):
    md_sections(body, where)
    ops = []
    position = 0
    matches = list(MD_TAG.finditer(body))
    index = 0
    while index < len(matches):
        match = matches[index]
        ops.append(("text", body[position:match.start()]))
        position = match.end()
        inner = match[1]
        if inner.startswith("example"):
            name = _md_attrs(inner[len("example"):], where).get("name", "").strip('"')
            end = next((m for m in matches[index + 1:] if m[1] == "/example"), None)
            if end is None:
                _fail(where, f"example {name!r} is not closed")
            ops.append(("example", name, dict(JSON_BLOCKS.findall(body[position:end.start()]))))
            position = end.end()
            index = matches.index(end) + 1
            continue
        if value := re.fullmatch(r"\$(" + PATH + ")", inner):
            ops.append(("val", value[1]))
        elif inner.startswith("if "):
            ops.append(("if", inner[3:].strip()))
        elif re.fullmatch(r"else\s*/", inner):
            ops.append(("else",))
        elif inner == "/if":
            ops.append(("endif",))
        elif loop := re.fullmatch(r"for (\w+) in (" + PATH + ")", inner):
            ops.append(("for", loop[1], loop[2]))
        elif inner == "/for":
            ops.append(("endfor",))
        elif inner.startswith("partial"):
            self_closing = inner.endswith("/")
            attrs = _md_attrs(inner[len("partial"):].rstrip("/"), where)
            file = attrs.pop("file", "").strip('"')
            if not file:
                _fail(where, "a partial needs file=\"...\"")
            ops.append(("partial" if self_closing else "partial_open", file, attrs))
        elif inner == "/partial":
            ops.append(("partial_close",))
        elif rendered := re.fullmatch(r"render \$(" + PATH + r")\s*/", inner):
            ops.append(("render", rendered[1]))
        elif re.fullmatch(r"slot\s*/", inner):
            ops.append(("slot",))
        else:
            _fail(where, f"tag {{% {inner} %}} is not one the catalog checker understands")
        index += 1
    ops.append(("text", body[position:]))
    return ops


# --- JSX-style tags: .atom.mdx ------------------------------------------------------

MDX_TOKEN = re.compile(
    r"(?P<fence>```.*?```)"
    r"|(?P<code>`[^`\n]*`)"
    r"|</(?P<close>[A-Za-z][\w.-]*)\s*>"
    r"|<(?P<open>[A-Za-z][\w.-]*)(?P<attrs>(?:[^<>{}]|\{[^{}]*\})*?)(?P<self>/)?>"
    r"|\{(?P<expr>[^{}]*)\}"
    r"|(?P<stray>[<{}])",
    re.S,
)
MDX_ATTR = re.compile(r'(\w+)=(?:("[^"]*")|\{([^{}]*)\})')


def _mdx_attrs(text, where):
    attrs = {}
    for key, quoted, braced in MDX_ATTR.findall(text):
        if key in attrs:
            _fail(where, f"attribute {key} is repeated")
        attrs[key] = quoted if quoted else braced.strip()
    return attrs


def mdx_ops(body, where):
    ops = []
    position = 0
    lowercase_open = []
    while match := MDX_TOKEN.search(body, position):
        ops.append(("text", body[position:match.start()]))
        position = match.end()
        if match["fence"] or match["code"]:
            ops.append(("text", match[0]))
        elif match["stray"]:
            line = body.count("\n", 0, match.start()) + 1
            _fail(where, f"line {line}: {match['stray']!r} outside code must start a tag or an expression; "
                         "put it in a code span or escape it")
        elif match["expr"] is not None:
            expression = match["expr"].strip()
            if not re.fullmatch(PATH, expression):
                _fail(where, f"{{{expression}}}: the catalog checker supports only value paths in braces")
            ops.append(("val", expression))
        elif match["close"]:
            name = match["close"]
            if name.startswith(RESERVED_TAG_PREFIX):
                _fail(where, f"tags starting with {RESERVED_TAG_PREFIX!r} are reserved for inserted values")
            if name[0].islower():
                if not lowercase_open or lowercase_open.pop() != name:
                    _fail(where, f"</{name}> does not close an open tag")
                ops.append(("text", match[0]))
            elif name == "If":
                ops.append(("endif",))
            elif name == "For":
                ops.append(("endfor",))
            elif name == "Partial":
                ops.append(("partial_close",))
            else:
                _fail(where, f"</{name}> is not a known tag")
        else:
            name, self_closing = match["open"], bool(match["self"])
            attrs = _mdx_attrs(match["attrs"], where)
            if name.startswith(RESERVED_TAG_PREFIX):
                _fail(where, f"tags starting with {RESERVED_TAG_PREFIX!r} are reserved for inserted values")
            if name[0].islower():
                # Lowercase tags are literal prompt sections; only their balance is checked.
                if not self_closing:
                    lowercase_open.append(name)
                ops.append(("text", match[0]))
            elif name == "If" and not self_closing and "cond" in attrs:
                ops.append(("if", attrs["cond"]))
            elif name == "Else" and self_closing:
                ops.append(("else",))
            elif name == "For" and not self_closing and {"each", "as"} <= attrs.keys():
                if not re.fullmatch(PATH, attrs["each"]):
                    _fail(where, "<For each={...}> must name a value path in this catalog")
                ops.append(("for", attrs["as"].strip('"'), attrs["each"]))
            elif name == "Partial" and "file" in attrs:
                file = attrs.pop("file").strip('"')
                ops.append(("partial" if self_closing else "partial_open", file, attrs))
            elif name == "Render" and self_closing and "value" in attrs:
                ops.append(("render", attrs["value"]))
            elif name == "Slot" and self_closing:
                ops.append(("slot",))
            elif name == "Example" and not self_closing:
                end = body.find("</Example>", position)
                if end < 0:
                    _fail(where, "<Example> is not closed")
                ops.append(("example", attrs.get("name", "").strip('"'), dict(JSON_BLOCKS.findall(body[position:end]))))
                position = end + len("</Example>")
            else:
                _fail(where, f"<{name}> is not a known tag, or is missing a required attribute")
    ops.append(("text", body[position:]))
    if lowercase_open:
        _fail(where, f"<{lowercase_open[-1]}> is never closed")
    return ops


# --- shared -------------------------------------------------------------------------

def ops_for(path, body):
    """Chooses the syntax by file extension: a file is one form or the other."""
    return mdx_ops(body, path) if path.name.endswith(".mdx") else md_ops(body, path)


def normalize(ops):
    """Collapses whitespace and merges text so two forms can be compared."""
    result = []
    for op in ops:
        if op[0] == "text":
            text = " ".join(op[1].split())
            if not text:
                continue
            if result and result[-1][0] == "text":
                result[-1] = ("text", result[-1][1] + " " + text)
            else:
                result.append(("text", text))
        elif op[0] == "if":
            result.append(("if", " ".join(op[1].split())))
        elif op[0] == "example":
            result.append(("example", op[1], {key: " ".join(value.split()) for key, value in op[2].items()}))
        else:
            result.append(op)
    return result
