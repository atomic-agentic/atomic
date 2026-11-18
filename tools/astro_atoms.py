# SPDX-License-Identifier: Apache-2.0
"""Use the shipped Astro compiler for catalog checks; never execute atom modules."""
from pathlib import Path
import json
import os
import subprocess
import shutil
import tempfile

ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / 'crates/atomic-core/assets'
CACHE = {}


def invoke(request):
    binary = os.environ.get('ATOMIC_DENO') or shutil.which('deno')
    if not binary:
        raise ValueError('Astro checks require pinned Deno 2.9.8; set ATOMIC_DENO to its executable')
    # As in atomic itself, Deno may read only the compiler assets: atom sources
    # travel in the request.
    result = subprocess.run([binary, 'run', '--no-config', '--no-lock', '--no-remote', '--cached-only', '--no-npm', '--no-prompt', f'--allow-read={ASSETS}', str(ASSETS / 'worker.mjs')], input=json.dumps(request), text=True, capture_output=True, timeout=35, env={'DENO_NO_UPDATE_CHECK': '1', 'DENO_DIR': os.environ.get('DENO_DIR') or os.path.join(tempfile.gettempdir(), 'atomic-catalog-deno')})
    if result.returncode:
        raise ValueError(f'the Astro compiler crashed: {result.stderr.strip()}')
    response = json.loads(result.stdout)
    if 'error' in response:
        error = response['error']
        if isinstance(error, dict):
            lines = [f"{d['file']}:{d['line']}:{d['column']}: {d['message']}" for d in error.get('diagnostics', [])]
            where = ':'.join(str(part) for part in (error.get('file'), error.get('line')) if part)
            raise ValueError('\n'.join(lines) or (f'{where}: ' if where else '') + error.get('message', 'compiler error'))
        raise ValueError(error)
    return response['ok']


def read_source(target, root):
    """Read one component the way atomic does: canonical, inside the root, .astro, bounded."""
    canonical = Path(target).resolve(strict=True)
    if canonical.suffix != '.astro' or not canonical.is_relative_to(root):
        raise ValueError(f'{target}: component must be an Astro source inside the project root')
    text = canonical.read_bytes()[:262_145].decode('utf-8')
    if len(text) > 262_144:
        raise ValueError(f'{target}: source size limit exceeded')
    return str(canonical), text


def load(path):
    path = path.resolve()
    if path not in CACHE:
        project = next((p for p in path.parents if (p / 'atomic.yaml').exists()), path.parent)
        source_root = project.parent if project.name == '.atomic' else project
        request = {'file': str(path), 'root': str(source_root), 'action': 'load', 'sources': {}, 'sourcePaths': {}}
        targets = [str(path)]
        # The same discovery loop as atomic: the compiler names each missing
        # component, the host reads it, and the compiler is asked again.
        for _ in range(33):
            for target in targets:
                canonical, text = read_source(target, source_root)
                request['sourcePaths'][target] = canonical
                request['sources'].setdefault(canonical, text)
            if sum(map(len, request['sources'].values())) > 1_048_576 or len(request['sources']) > 128:
                raise ValueError(f'{path}: component graph limit exceeded')
            result = invoke(request)
            if 'needsSources' not in result:
                break
            targets = result['needsSources']
        else:
            raise ValueError(f'{path}: component graph limit exceeded')
        CACHE[path] = (request | {'sources': result['sources'], 'sourcePaths': result['sourcePaths']}, result['config'])
    return CACHE[path]


def inline(schema, resolver, depth=0):
    if depth > 64:
        raise ValueError('recursive schema is outside catalog type generation')
    if isinstance(schema, dict):
        if '$id' in schema:
            from referencing import Resource
            from referencing.jsonschema import DRAFT202012
            resolver = resolver.in_subresource(Resource.from_contents(schema, default_specification=DRAFT202012))
        if '$ref' in schema:
            found = resolver.lookup(schema['$ref'])
            schema = found.contents | {k:v for k,v in schema.items() if k != '$ref'}
            resolver = found.resolver
        return {k:inline(v, resolver, depth+1) for k,v in schema.items() if k not in ('$id','$defs','$schema')}
    if isinstance(schema, list):
        return [inline(v,resolver,depth+1) for v in schema]
    return schema


def check(path, input_schema, output_schema, registry, input_uri, output_uri):
    request, config = load(path)
    schemas = {'Input':inline(input_schema,registry.resolver(input_uri)), 'Output':inline(output_schema,registry.resolver(output_uri)), 'Context':{'type':'object','additionalProperties':False}, 'Case':{}}
    sidecars = list(path.parent.glob(path.name.removesuffix('.atom.astro') + '.context.schema.*'))
    if len(sidecars) > 1: raise ValueError(f'{path}: ambiguous context sidecar')
    if sidecars:
        import yaml
        schemas['Context'] = inline(yaml.safe_load(sidecars[0].read_text()), registry.resolver(sidecars[0].as_uri()))
    declarations = invoke(request | {'action':'check','schemas':schemas,'authoring':(ROOT / 'authoring/authoring.d.ts').read_text()})
    for name, text in declarations.items():
        target = Path(name)
        target.parent.mkdir(parents=True,exist_ok=True)
        target.write_text(text)
    return config


def fixture(path, input_value, output_value):
    request, _ = load(path)
    options = {'runId':'catalog','origin':'external'}
    settings = Path(request['root']) / 'context.settings.json'
    if settings.is_file():
        host = json.loads((settings.parent/'context.fixture.json').read_text())
        import yaml
        sidecar = next(path.parent.glob(path.name.removesuffix('.atom.astro') + '.context.schema.*'))
        schema = yaml.safe_load(sidecar.read_text())
        snapshot = invoke(request | {'action':'resolveContext','programHash':'catalog','environment':{item['name']:host['environment'][item['name']] for item in json.loads(settings.read_text())['environment'].values() if item['name'] in host['environment']},'resolution':{'document':{'$schema':'https://json-schema.org/draft/2020-12/schema','$defs':{'RuntimeContext':schema}},'settings':json.loads(settings.read_text()),'profile':host['profile'],'metadata':host['metadata']}})
        options = {'runId':snapshot['runId'],'origin':'external','contextSnapshot':snapshot}
    rendered = invoke(request | {'action':'render','input':input_value,'options':options})
    result = invoke(request | {'action':'validate','values':{'input':input_value,'output':output_value},'options':options})
    if result.get('valid') is not True:
        raise ValueError(f'{path}: golden output fails interpreted validators: {result}')
    return rendered
