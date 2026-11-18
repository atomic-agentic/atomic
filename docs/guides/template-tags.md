# Prompt expressions and components

Atomic authors declarative prompts in `.atom.astro`. Start with generated `Props`
and bind `Astro.props` explicitly. The source is parsed and interpreted; it is never
executed as an Astro page or imported JavaScript module.

```astro
---
import type { Props } from '../.atomic/types/triage-findings';
import TriageRules from './TriageRules.astro';
import Finding from './Finding.astro';
const { input }: Props = Astro.props;
---
<TriageRules strict={true}>
Findings marked blocker always go in must_fix.
</TriageRules>

Work item {input.item.id}: {input.item.title}

<criteria>
{input.item.criteria.map(c => <Fragment>
- {c}
</Fragment>)}
</criteria>

{input.findings.length === 0 ? <Fragment>
No findings were reported.
</Fragment> : <Fragment>
Findings:
{input.findings.map(f => <Finding finding={f} />)}
</Fragment>}
```

The [complete example](../../examples/08-templating/atoms/triage-findings.atom.astro)
uses the same compiler as production. Schema-backed types catch missing properties,
wrong component props and invalid validator results. `atomic check` also enforces
the interpreter subset and complete runtime schemas.

## Components and slots

A local `.astro` component declares a `Props` interface and destructures its props.
A `<slot />` receives the caller's nested prompt content.

```astro
---
interface Props { strict: boolean }
const { strict }: Props = Astro.props;
---
Sort the findings into immediate fixes and follow-up work.
{strict ? <Fragment>A missing test is a blocker.</Fragment> : null}
<slot />
```

Import components explicitly by their local filename. They share the atom's source
containment rules and are captured in its compiled source graph. Cycles and imports
that escape the project fail. There is no npm component execution, implicit
schema-bound renderer lookup or separate partial parameter language.

## Data and prompt text

Lowercase section tags such as `<criteria>` organize the prompt. `<Fragment>` groups
content without adding a section. Interpolated values retain their input provenance
and appear in tagged user bindings, while static text remains developer instructions.
Components and expressions preserve that separation.

Use entities for literal angle brackets and braces: `&lt;module>` and
`&#123;placeholder&#125;`. Escape code examples too: prompt bodies do not acquire
Markdown's code-fence parsing rules. Formatting changes prompt whitespace; review
formatter output before accepting it.

The supported expression and resource contract is in [atom files](../reference/atom-files.md).
YAML workflows still use CEL for `with`, `returns` and gate displays.
