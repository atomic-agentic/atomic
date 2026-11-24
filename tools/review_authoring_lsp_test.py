#!/usr/bin/env python3
"""Headless stock Astro language-server smoke for the review pilot."""
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import tempfile
import threading
import time

from review_authoring_pilot_test import SOURCE, ATOMIC, run, inferred as make_inferred

NODE = os.environ.get('NODE_BIN', 'node')
MODULES = Path(os.environ.get('ASTRO_NODE_MODULES', 'authoring/node_modules')).resolve()
SERVER = MODULES / '@astrojs/language-server/bin/nodeServer.js'
TSDK = MODULES / 'typescript/lib'


class Lsp:
    def __init__(self, root):
        self.proc = subprocess.Popen([NODE, str(SERVER), '--stdio'], cwd=root,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE)
        self.received = queue.Queue()
        self.next_id = 0
        threading.Thread(target=self._reader, daemon=True).start()
        self.request('initialize', {
            'processId': None, 'rootUri': root.as_uri(),
            'workspaceFolders': [{'uri': root.as_uri(), 'name': 'review-pilot'}],
            'capabilities': {'workspace': {'configuration': True, 'workspaceFolders': True},
                             'textDocument': {'diagnostic': {'relatedDocumentSupport': True},
                                              'completion': {'completionItem': {'snippetSupport': True}},
                                              'hover': {'contentFormat': ['markdown', 'plaintext']}}},
            'initializationOptions': {'typescript': {'tsdk': str(TSDK)}},
        })
        self.notify('initialized', {})

    def _reader(self):
        while True:
            headers = {}
            while line := self.proc.stdout.readline():
                if line == b'\r\n':
                    break
                k, _, v = line.decode().partition(':')
                headers[k.lower()] = v.strip()
            if not headers:
                return
            size = int(headers['content-length'])
            message = json.loads(self.proc.stdout.read(size))
            if message.get('method') == 'workspace/configuration':
                self.send({'jsonrpc': '2.0', 'id': message['id'],
                           'result': [None] * len(message['params']['items'])})
            elif 'id' in message:
                self.received.put(message)

    def send(self, message):
        payload = json.dumps(message).encode()
        self.proc.stdin.write(f'Content-Length: {len(payload)}\r\n\r\n'.encode() + payload)
        self.proc.stdin.flush()

    def request(self, method, params):
        self.next_id += 1
        ident = self.next_id
        self.send({'jsonrpc': '2.0', 'id': ident, 'method': method, 'params': params})
        deadline = time.monotonic() + 25
        while time.monotonic() < deadline:

            try:
                response = self.received.get(timeout=min(1, max(0.1, deadline - time.monotonic())))
            except queue.Empty:
                if self.proc.poll() is not None:
                    raise AssertionError(f'LSP exited {self.proc.returncode}: {self.proc.stderr.read().decode()}')
                continue
            if response['id'] == ident:
                if 'error' in response:
                    raise AssertionError(response['error'])
                return response.get('result')
        raise TimeoutError(method)

    def notify(self, method, params):
        self.send({'jsonrpc': '2.0', 'method': method, 'params': params})

    def close(self):
        try:
            self.request('shutdown', None)
            self.notify('exit', None)
        finally:
            self.proc.kill()
            self.proc.wait(timeout=3)


def pos(text, needle, shift=0):
    offset = text.index(needle) + shift
    before = text[:offset].split('\n')
    return {'line': len(before) - 1, 'character': len(before[-1])}


def errors(lsp, uri):
    return [item for item in lsp.request('textDocument/diagnostic',
            {'textDocument': {'uri': uri}})['items'] if item.get('severity') == 1]


def exercise(root, inferred_mode=False):
    if inferred_mode:
        root.mkdir(parents=True, exist_ok=True)
        make_inferred(root)
        project = root / 'inferred'
        atom = project / 'self-review.atom.astro'
    else:
        root.mkdir(parents=True, exist_ok=True)
        project = root / 'project'
        shutil.copytree(SOURCE, project, ignore=shutil.ignore_patterns('.atomic'))
        assert run(project, 'check', '--json')['checked']['atoms'] == 2
        atom = project / 'atoms/self-review.atom.astro'
    text = atom.read_text()
    uri = atom.as_uri()
    lsp = Lsp(project)
    try:
        lsp.notify('textDocument/didOpen', {'textDocument':
                   {'uri': uri, 'languageId': 'astro', 'version': 1, 'text': text}})
        assert not errors(lsp, uri), 'valid pilot has editor errors'
        result = lsp.request('textDocument/completion', {'textDocument': {'uri': uri},
                             'position': pos(text, 'input.packet', len('input.'))})
        labels = [item['label'].rstrip('?') for item in (result.get('items', []) if isinstance(result, dict) else result)]
        assert 'packet' in labels, labels
        hover = lsp.request('textDocument/hover', {'textDocument': {'uri': uri},
                            'position': pos(text, 'input.packet', len('input.packet') - 2)})
        assert 'candidate' in json.dumps(hover) and 'checks' in json.dumps(hover), hover
        source = lsp.request('textDocument/definition', {'textDocument': {'uri': uri},
                             'position': pos(text, 'input.packet', len('input.') + 2)})
        assert '.atomic/types/' in json.dumps(source), source
        declaration = (project / '.atomic/types/schemas/review-packet.d.ts').read_text()
        assert '@atomic-schema schemas/review-packet.schema.yaml#/properties/candidate' in declaration
        component = lsp.request('textDocument/definition', {'textDocument': {'uri': uri},
                                'position': pos(text, '<ReviewPacket packet', 2)})
        assert 'ReviewPacket.astro' in json.dumps(component), component
        bad_prop = text.replace('<ReviewPacket packet={input.packet}', '<ReviewPacket packett={input.packet}')
        lsp.notify('textDocument/didChange', {'textDocument': {'uri': uri, 'version': 2},
                   'contentChanges': [{'text': bad_prop}]})
        assert any('packett' in e['message'] for e in errors(lsp, uri)), 'wrong prop not flagged'
        # A validator for self-review cannot read the review-plan output contract.
        cross = text.replace('output.findings.length === 0', 'output.steps.length === 0')
        lsp.notify('textDocument/didChange', {'textDocument': {'uri': uri, 'version': 3},
                   'contentChanges': [{'text': cross}]})
        assert any('steps' in e['message'] for e in errors(lsp, uri)), 'cross-atom output not rejected'
        atom.write_text(cross)
        env = dict(os.environ, ATOMIC_HOME=str(root / 'atomic-state'))
        checked = subprocess.run([ATOMIC, 'check', *(['self-review.atom.astro'] if inferred_mode else []), '--json'], cwd=project, env=env,
                                 text=True, capture_output=True)
        assert checked.returncode and 'steps' in checked.stdout, checked.stdout
        atom.write_text(text)
        # Rename the source schema. Atomic's checker stops at its first error,
        # so check each configured importer separately and assert its source site.
        schema = project / 'schemas/review-packet.schema.yaml'
        schema.rename(project / 'schemas/review-packet-renamed.schema.yaml')
        importers = ['self-review.atom.astro'] if inferred_mode else [
            'atoms/self-review.atom.astro', 'atoms/review-plan.atom.astro']
        for importer in importers:
            checked = subprocess.run([ATOMIC, 'check', importer, '--json'],
                                     cwd=project, env=env, text=True, capture_output=True)
            assert checked.returncode, checked.stdout
            description = json.loads(checked.stdout)['error']['description']
            assert importer in description and 'schemas/review-packet.schema.yaml' in description, description
        return {'completion': labels, 'hover': hover, 'source_definition': source, 'component_definition': component,
                'wrong_prop': True, 'cross_output_validator': True, 'schema_rename': True}
    finally:
        lsp.close()


if __name__ == '__main__':
    assert SERVER.exists(), SERVER
    assert TSDK.exists(), TSDK
    with tempfile.TemporaryDirectory(prefix='atomic-review-lsp-') as tmp:
        root = Path(tmp)
        report = {'configured': exercise(root / 'configured-case'),
                  'inferred': exercise(root / 'inferred-case', inferred_mode=True)}
    node_version = subprocess.run([NODE, '--version'], text=True, capture_output=True, check=True).stdout.strip()
    packages = {name: json.loads((MODULES / name / 'package.json').read_text())['version']
                for name in ('@astrojs/language-server', 'typescript')}
    print(json.dumps({'node': node_version, 'server': str(SERVER),
                      'language_server': packages['@astrojs/language-server'],
                      'typescript': packages['typescript'], 'checks': report}, indent=2))
