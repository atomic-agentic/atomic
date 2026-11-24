// The layout variants present one fixed set of findings in different shapes.
// Everything a shape needs beyond interpolation lives here, so a component stays
// a declaration of layout. A helper sees only its arguments, so a value computed
// from a finding stays external data in the rendered prompt, and the finding it
// came from stays named by the call's path.
//
// The finding shape is written here as it is in the components that render it,
// and the atom passes its own findings: what the input schema declares is what
// the type checker compares against on both sides.
interface Finding {
  file: string;
  severity: string;
  message: string;
}

// One run of findings reported against the same file: `file` names the run and
// `findings` holds its findings in the order they were reported.
export interface FindingRun {
  file: string;
  findings: Finding[];
}

// The findings cut into runs by position: a run begins wherever the finding
// before it names another file, and a run ends wherever the next finding names
// another file. So a file named again after another file's findings opens its own
// run there, instead of pulling its rows back to its first run, and two identical
// findings reported side by side are one run holding two findings, so a layout
// that prints one header per run prints it once.
//
// Runs are cut by position rather than matched by value because a grouped layout
// has to know where a finding was reported, and the finding a layout is rendering
// names no position: matched by value, a finding that repeats is the same answer
// at every place it appears. This makes the cut in report order, so nothing is
// merged, reordered, dropped or added: the findings of all the runs, read in
// order, are exactly the findings that were passed in.
export const findingRuns = (findings: readonly Finding[]): FindingRun[] => {
  const runs: FindingRun[] = [];
  for (let index = 0; index < findings.length; index += 1) {
    const finding = findings[index];
    const previous = runs.length === 0 ? null : runs[runs.length - 1];
    if (previous === null || previous.file !== finding.file) {
      runs.push({ file: finding.file, findings: [finding] });
    } else {
      previous.findings.push(finding);
    }
  }
  return runs;
};

// One GitHub-flavoured Markdown table cell holding `text`: the table's own
// syntax escaped and its line breaks folded, so one finding stays one row.
// The fold is lossy on purpose, which is why layout C keeps the unescaped text
// beside the table rather than dropping it here.
export const cell = (text: string): string =>
  text
    .replace(/\\/g, '\\\\')
    .replace(/\|/g, '\\|')
    .replace(/[\r\n\t]+/g, ' ');
