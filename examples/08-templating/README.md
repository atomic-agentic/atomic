# Prompt composition with Astro

[The triage atom](atoms/triage-findings.atom.astro) demonstrates the single Astro
format: typed values, conditions, loops and explicit local components.

- [TriageRules](atoms/TriageRules.astro) accepts a typed `strict` prop and caller content through `<slot />`.
- [Finding](atoms/Finding.astro) renders a typed finding; it replaces implicit schema-bound renderer lookup.
- The atom maps over criteria and findings and selects text based on list length.
- Its typed validator rejects an accepting decision that still contains required fixes.

Run `atomic check` to regenerate `.atomic/types` and check the source with the
production parser, type checker and full schemas. The catalog checker also renders
the fixture and executes the validator. Both use the shipped compiler; no alternate
Markdown/MDX parser or approximate Python template implementation remains.

Values retain their provenance through components and slots. Literal angle brackets
in prose use entities such as `&lt;module>`. Whitespace is prompt content; format
Astro source deliberately.
