#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Check that Claude Code and Codex list the skill `atomic setup` installs.
#
#   scripts/agent-skills-check.sh [path/to/atomic]
#
# Runs `atomic setup claude` and `atomic setup codex` in fresh temporary
# directories, then asks each installed agent which skills it sees there:
# Codex through `codex debug prompt-input`, which renders the model's input
# without calling a model, and Claude Code through the `init` message of a
# print-mode session, which is read and then cut off. The Claude Code check
# starts a signed-in session, so it needs the agent's own login.
#
# Not run in CI: it needs both agents installed and signed in. A missing agent
# is a failure, never a skip.
#
# Environment:
#   CLAUDE   the Claude Code executable (default: claude)
#   CODEX    the Codex executable (default: codex)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ATOMIC="${1:-${ROOT}/target/debug/atomic}"
CLAUDE="${CLAUDE:-claude}"
CODEX="${CODEX:-codex}"
SKILL=atomic-create-atom

for tool in "${ATOMIC}" "${CLAUDE}" "${CODEX}"; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "agent-skills-check: ${tool} is not installed" >&2
        exit 1
    fi
done

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT
export ATOMIC_HOME="${work}/atomic-home"
status=0

mkdir -p "${work}/codex"
"${ATOMIC}" setup codex --path "${work}/codex" >/dev/null
if (cd "${work}/codex" && timeout 120 "${CODEX}" debug prompt-input) | grep -q "${SKILL}: "; then
    echo "codex $("${CODEX}" --version): lists ${SKILL}"
else
    echo "codex: does not list ${SKILL} from .agents/skills" >&2
    status=1
fi

mkdir -p "${work}/claude"
"${ATOMIC}" setup claude --path "${work}/claude" >/dev/null
init="$(cd "${work}/claude" && timeout 120 "${CLAUDE}" -p "Reply with OK." \
    --output-format stream-json --verbose --max-turns 1 2>/dev/null | head -n 1 || true)"
if printf '%s' "${init}" | grep -q "\"${SKILL}\""; then
    echo "claude $("${CLAUDE}" --version): lists ${SKILL}"
else
    echo "claude: does not list ${SKILL} from .claude/skills" >&2
    status=1
fi

exit "${status}"
