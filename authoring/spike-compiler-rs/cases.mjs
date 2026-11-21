// SPDX-License-Identifier: Apache-2.0
// Syntax cases beyond the catalog: every construct atomic's renderer lowers or
// rejects, and sources the compilers should refuse. `invalid` marks sources
// that atomic must not accept, by the parser or, later, by the renderer.
const fm = (body, script = "const { input } = Astro.props;") => `---\n${script}\n---\n${body}`;

export const cases = [
  { name: 'text-and-expression', source: fm('Note: {input.text}\n') },
  { name: 'html-comment', source: fm('<!-- hidden -->\nVisible {input.a}\n') },
  { name: 'expression-comment', source: fm('{/* nothing */}{input.a}\n') },
  { name: 'empty-expression', source: fm('A{}B\n') },
  { name: 'nested-jsx-in-expression', source: fm('{input.items.map((i) => <Fragment>\n- {i}\n</Fragment>)}\n') },
  { name: 'conditional-jsx', source: fm('{input.ok ? <Fragment>yes</Fragment> : <Fragment>no {input.why}</Fragment>}\n') },
  { name: 'short-fragment', source: fm('<>\ninside {input.a}\n</>\n') },
  { name: 'default-slot', source: fm('<section>\n<slot />\n</section>\n') },
  { name: 'named-slot', source: fm('<slot name="extra" />\n') },
  { name: 'section-tags', source: fm('<criteria>\nOne\n</criteria>\n<notes>\nTwo\n</notes>\n') },
  { name: 'component-attrs', source: fm('<Card title="t" strict={true} count={input.n} />\n', 'import Card from "./Card.astro";\nconst { input } = Astro.props;') },
  { name: 'component-children', source: fm('<Card>\nBody {input.a}\n</Card>\n', 'import Card from "./Card.astro";\nconst { input } = Astro.props;') },
  { name: 'member-component', source: fm('<ui.Card />\n') },
  { name: 'shorthand-attr', source: fm('<Card {input} />\n', 'import Card from "./Card.astro";\nconst { input } = Astro.props;') },
  { name: 'spread-attr', source: fm('<Card {...input} />\n', 'import Card from "./Card.astro";\nconst { input } = Astro.props;') },
  { name: 'empty-attr', source: fm('<Card strict />\n', 'import Card from "./Card.astro";\nconst { input } = Astro.props;') },
  { name: 'template-literal-attr', source: fm('<Card title=`t ${input.a}` />\n', 'import Card from "./Card.astro";\nconst { input } = Astro.props;') },
  { name: 'directive-attr', source: fm('<Card client:load set:html={input.a} />\n', 'import Card from "./Card.astro";\nconst { input } = Astro.props;') },
  { name: 'script-element', source: fm('<script>console.log(1 < 2)</script>\n') },
  { name: 'style-element', source: fm('<style>p { color: red }</style>\n') },
  { name: 'reserved-data-element', source: fm('<data-x>y</data-x>\n') },
  { name: 'entities', source: fm('Write &lt;name> and &#123;value&#125; and &amp;.\n') },
  { name: 'raw-greater-than', source: fm('If a > b then say so.\n') },
  { name: 'non-ascii', source: fm('Café — “quotes” {input.a} ünïcødé 🙂 {input.b}\n') },
  { name: 'crlf-line-endings', source: '---\r\nconst { input } = Astro.props;\r\n---\r\nLine one {input.a}\r\nLine two {input.b}\r\n' },
  { name: 'no-frontmatter', source: 'Plain prompt {1 + 1}\n' },
  { name: 'multiline-expression', source: fm('{\n  input.a\n    ? "x"\n    : "y"\n}\n') },
  { name: 'multiline-attr', source: fm('<Card\n  title="t"\n  value={input.a}\n/>\n', 'import Card from "./Card.astro";\nconst { input } = Astro.props;') },
  { name: 'doctype', source: '<!doctype html>\n<p>x</p>\n' },
  { name: 'void-element', source: fm('Line<br>next\n') },
  { name: 'markdown-like-prose', source: fm('# Heading\n\n- item one\n- item **two**\n\n```js\nconst a = 1;\n```\n') },
  // Invalid: atomic must refuse each of these somewhere.
  // Valid Astro that atomic refuses (directives, scripts, spreads...) stays unmarked: both parsers must lead to the same refusal.
  { name: 'unclosed-element', invalid: true, source: fm('<section>\nNever closed {input.a}\n') },
  { name: 'mismatched-close', invalid: true, source: fm('<section>\nx\n</notes>\n') },
  { name: 'stray-close', invalid: true, source: fm('text\n</section>\nmore\n') },
  { name: 'unclosed-expression', invalid: true, source: fm('Value: {input.a\nmore text\n') },
  { name: 'stray-close-brace', invalid: true, source: fm('Value } here\n') },
  { name: 'bad-js-in-expression', invalid: true, source: fm('Value: {input.a +}\n') },
  { name: 'unterminated-frontmatter', invalid: true, source: '---\nconst { input } = Astro.props;\nNote: {input.text}\n' },
  { name: 'bad-ts-in-frontmatter', invalid: true, source: fm('Note\n', 'const { input } = Astro.props\nconst = 1;') },
  { name: 'second-frontmatter', invalid: true, source: fm('Note\n---\nconst x = 1;\n---\n') },
  { name: 'stray-less-than', invalid: true, source: fm('If a < b then say so.\n') },
  { name: 'unclosed-attr-quote', invalid: true, source: fm('<Card title="t />\n', 'import Card from "./Card.astro";\nconst { input } = Astro.props;') },
  { name: 'unclosed-comment', invalid: true, source: fm('<!-- never closed\nmore\n') },
];
