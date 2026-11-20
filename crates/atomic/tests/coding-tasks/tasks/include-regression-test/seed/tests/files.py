"""Synthetic deploy files in a temporary directory, for the tests."""

import os
import tempfile
import textwrap

CLEAN = """\
[service]
name = demo-api
image = registry.example.internal/demo/api:1.0.3

[deploy]
env = staging
replicas = 2

[http]
port = 8000
"""


class TempTree:
    """A temporary directory that tests write deploy files into. Use it as a context manager."""

    def __enter__(self):
        self._dir = tempfile.TemporaryDirectory()
        self.root = os.path.realpath(self._dir.name)
        return self

    def __exit__(self, *exc):
        self._dir.cleanup()

    def write(self, relative, text):
        path = os.path.join(self.root, relative)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as f:
            f.write(textwrap.dedent(text))
        return path
