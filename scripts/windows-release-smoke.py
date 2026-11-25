#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Exercise the extracted release through Codex's Windows sandbox and a scripted Responses API."""
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from zipfile import ZipFile

ROOT = Path(__file__).resolve().parents[1]


def coding_turn(exe, project, repository, state, env):
    """The second model call must carry proof of a workspace write and an ACL refusal."""
    canary = state / "private" / "smoke.txt"
    canary.parent.mkdir(parents=True, exist_ok=True)
    canary.write_text(secrets.token_hex(32))
    expected = hashlib.sha256((repository / "pricing.py").read_bytes()).hexdigest().upper()
    # cmd.exe uses the worktree assigned to the Windows process. Built-in
    # TYPE must report access denied for the existing private file.
    command = (
        'copy /b pricing.py windows-copy.py >nul && '
        'certutil -hashfile windows-copy.py SHA256 && '
        f'type "{canary}" >nul'
    )
    shell = str(Path(os.environ["SystemRoot"]) / "System32/cmd.exe")
    output = (project / "fixtures/implement-step/quantity.output.json").read_text()
    replies = []

    class Responses(BaseHTTPRequestHandler):
        def do_POST(self):
            if self.path != "/v1/responses":
                self.send_error(404)
                return
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if not replies:
                item = dict(type="function_call", id="fc_1", call_id="call_1", name="exec_command",
                            arguments=json.dumps({"cmd": command, "shell": shell, "yield_time_ms": 10000}), status="completed")
            elif len(replies) == 1 and any(
                part.get("type") == "function_call_output" and part.get("call_id") == "call_1"
                and expected in str(part.get("output")).upper()
                and "Access is denied" in str(part.get("output"))
                and "Process exited with code 1" in str(part.get("output"))
                for part in request.get("input", []) if isinstance(part, dict)
            ):
                item = dict(type="message", id="msg_1", role="assistant", status="completed",
                            content=[dict(type="output_text", text=output, annotations=[])])
            else:
                for part in request.get("input", []):
                    if isinstance(part, dict) and part.get("type") == "function_call_output":
                        print(f"Sandboxed shell returned {str(part.get('output'))[:2000]}", flush=True)
                self.send_error(400, "Missing evidence from the sandboxed shell")
                return
            replies.append(item["type"])
            response = dict(id=f"resp_{len(replies)}", object="response", model=request["model"],
                            status="completed", output=[item],
                            usage=dict(input_tokens=1, output_tokens=1, total_tokens=2))
            events = [
                ("response.created", dict(response={**response, "status": "in_progress", "output": []})),
                ("response.output_item.added", dict(output_index=0, item=item)),
                ("response.output_item.done", dict(output_index=0, item=item)),
                ("response.completed", dict(response=response)),
            ]
            body = "".join(f"event: {name}\ndata: {json.dumps(dict(type=name, **data))}\n\n"
                           for name, data in events).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    workflow = project / "workflows" / "windows-smoke.workflow.yaml"
    workflow.write_text("""atomic: 1
input:
  $ref: ../atoms/implement-step.input.schema.yaml
output:
  $ref: ../atoms/implement-step.output.schema.yaml
steps:
  implement:
    atom: implement-step
    worktree: true
    with:
      task: input.task
      plan: input.plan
      base: input.base
returns: steps.implement.output
""")
    with ThreadingHTTPServer(("127.0.0.1", 0), Responses) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            subprocess.run([exe, "run", "windows-smoke", "--input", "fixtures/implement-step/quantity.input.json",
                            "--fixture", repository, "--fixture-rev", "baseline",
                            "--model", "synthetic", "--model-base-url", f"http://127.0.0.1:{server.server_port}/v1"],
                           check=True, cwd=project, env=env, timeout=300)
        finally:
            server.shutdown()
            thread.join()
    if replies != ["function_call", "message"]:
        raise RuntimeError(f"Expected a sandboxed tool call and final answer, got {replies}")


def main():
    archive, sbom = Path(sys.argv[1]), Path(sys.argv[2])
    # Exercise a normally created directory with spaces under the operator profile.
    scratch = Path.home() / ("atomic Windows smoke " + secrets.token_hex(6))
    scratch.mkdir()
    try:
        with ZipFile(archive) as bundle:
            bundle.extractall(scratch)
        exe = scratch / archive.stem / "atomic.exe"
        metadata = json.loads(sbom.read_text(encoding="utf-8"))["metadata"]["component"]
        app, = [c for c in metadata["components"] if c["name"] == "atomic" and c["type"] == "application"]
        digest = hashlib.sha256(exe.read_bytes()).hexdigest()
        if {"alg": "SHA-256", "content": digest} not in app["hashes"]:
            raise ValueError("SBOM does not match the extracted atomic.exe")
        dest = scratch / "fixture"
        subprocess.run([sys.executable, ROOT / "examples/02-change-request/tools/prepare_fixture.py", dest], check=True)
        project = dest / "project"
        state = scratch / "state"
        # The preview needs a model identity to select Codex's instructions.
        # Render must work with this loopback endpoint offline. Live turns
        # override it with the scripted server's address below.
        env = {**os.environ, "ATOMIC_HOME": str(state), "ATOMIC_MODEL": "synthetic",
               "ATOMIC_MODEL_BASE_URL": "http://127.0.0.1:1/v1"}
        # Native PowerShell does not normally set the Unix HOME variable.
        env.pop("HOME", None)
        system = Path(os.environ["SystemRoot"])
        env["PATH"] = os.pathsep.join(map(str, [
            system / "System32", system, system / "System32/WindowsPowerShell/v1.0",
            Path(os.environ["ProgramFiles"]) / "Git/cmd",
        ]))
        for args in [["--version"], ["check"],
                     ["render", "atoms/implement-step.atom.astro", "--input", "fixtures/implement-step/quantity.input.json"]]:
            subprocess.run([exe, *args], cwd=project, env=env, check=True, timeout=120)
        context_project = ROOT / "examples/12-execution-context"
        render_context = [exe, "render", "atoms/context-reply.atom.astro", "--input",
                          "fixtures/context-reply/ordinary.input.json", "--context-settings",
                          "context.settings.json", "--profile", "test", "--run-id",
                          "windows-context-smoke"]
        for region in ["eu-west", "us-east"]:
            result = subprocess.run(render_context, cwd=context_project,
                                    env={**env, "SUPPORT_REGION": region}, check=True,
                                    capture_output=True, text=True, timeout=120)
            if "Region: eu-west" not in result.stdout or "Region: us-east" in result.stdout:
                raise AssertionError("Direct invocation did not reuse its frozen context")
        # Reuse the project to cover Codex's already-provisioned helper cache.
        for _ in range(2):
            coding_turn(exe, project, dest / "repository", state, env)

    finally:
        # Codex can still hold a setup helper open for a background read-ACL refresh.
        # The disposable runner releases it when the VM shuts down.
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    main()
