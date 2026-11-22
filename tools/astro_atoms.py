# SPDX-License-Identifier: Apache-2.0
"""Use the shipped Astro compiler for catalog checks; never execute atom modules."""
from pathlib import Path
import json
import os
import subprocess

ROOT = Path(__file__).resolve().parents[1]
CACHE = {}


def compiler():
    """The atomic binary whose embedded compiler runs the checks: ATOMIC_BIN, else this checkout's debug build."""
    binary = Path(os.environ.get('ATOMIC_BIN') or ROOT / 'target/debug/atomic')
    if not binary.is_file():
        raise ValueError(f'Astro checks run on the compiler embedded in atomic, and {binary} is not built; run cargo build or set ATOMIC_BIN')
    return binary


def invoke(request):
    # As in atomic itself: atomic runs its own embedded compiler as a child,
    # with a cleared environment; atom sources travel in the request.
    result = subprocess.run([str(compiler()), '__atomic-compiler'], input=json.dumps(request), text=True, capture_output=True, timeout=35, env={})
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


# A mirror of atomic's TypeScript projection (atomic_schema::typescript), so this
# checker needs no Rust build. It is tested against the same cases as the Rust
# generator (crates/atomic-schema/tests/typescript/projection.json); atomic's own
# declarations always come from Rust.
PROJECTION_CASES = ROOT / 'crates/atomic-schema/tests/typescript/projection.json'


def _literal(value):
    return json.dumps(value, separators=(',', ':'), ensure_ascii=False)


def schema_type(schema, depth=0):
    if depth > 64:
        raise ValueError('schema type depth limit exceeded')
    if schema is True:
        return 'unknown'
    if schema is False:
        return 'never'
    if not isinstance(schema, dict):
        raise ValueError('invalid schema')
    if 'const' in schema:
        return _literal(schema['const'])
    if 'enum' in schema:
        if not isinstance(schema['enum'], list):
            raise ValueError('enum must be an array')
        return ' | '.join(_literal(value) for value in schema['enum']) or 'never'
    combinators = [('anyOf', ' | '), ('oneOf', ' | '), ('allOf', ' & ')]
    if any(key in schema for key, _ in combinators):
        groups = []
        for key, separator in combinators:
            if key not in schema:
                continue
            if not isinstance(schema[key], list):
                raise ValueError(f'{key} must be an array')
            groups.append('(' + separator.join(schema_type(branch, depth + 1) for branch in schema[key]) + ')')
        combined = groups[0] if len(groups) == 1 else '(' + ' & '.join(groups) + ')'
        base = {key: value for key, value in schema.items() if key not in ('anyOf', 'oneOf', 'allOf')}
        return f'({schema_type(base, depth + 1)} & {combined})' if 'type' in base or 'properties' in base else combined
    if schema.get('$ref') == 'atomic:artifact':
        return 'string'
    if isinstance(schema.get('type'), list):
        return ' | '.join('null' if item == 'null' else schema_type(schema | {'type': item}, depth + 1) for item in schema['type'])
    if '$ref' in schema:
        raise ValueError('type generation requires resolved schema references')
    kind = schema.get('type')
    if kind == 'object' or 'properties' in schema:
        required = {key for key in schema.get('required', []) if isinstance(key, str)}
        properties = schema.get('properties', {})
        if not isinstance(properties, dict):
            raise ValueError('properties must be an object')
        members = [f"{_literal(key)}{'' if key in required else '?'}: {schema_type(value, depth + 1)};" for key, value in properties.items()]
        if schema.get('additionalProperties') is not False:
            members.append('[key: string]: unknown;')
        return '{ ' + ' '.join(members) + ' }'
    if kind == 'array':
        if 'prefixItems' not in schema:
            return f"Array<{schema_type(schema.get('items', True), depth + 1)}>"
        if not isinstance(schema['prefixItems'], list):
            raise ValueError('prefixItems must be an array')
        members = [schema_type(item, depth + 1) for item in schema['prefixItems']]
        if 'items' not in schema:
            members.append('...Array<unknown>')
        elif schema['items'] is not False:
            members.append(f"...Array<{schema_type(schema['items'], depth + 1)}>")
        return '[' + ', '.join(members) + ']'
    return {'string': 'string', 'number': 'number', 'integer': 'number', 'boolean': 'boolean', 'null': 'null'}.get(kind, 'unknown')


def atom_declarations(schemas):
    types = '\n'.join(f"export type {'ContextData' if key == 'Context' else key} = {schema_type(schema)};" for key, schema in schemas.items())
    return ('// Generated from the full runtime contracts. Do not edit.\n' + types + '\n'
            'export type Context = Immutable<ContextData>;\n'
            'export type Reply = Output;\n'
            'export type EvalCase = Case;\n'
            'export type Immutable<T> = T extends object ? { readonly [K in keyof T]: Immutable<T[K]> } : T;\n'
            'export interface Props { input: Input; readonly context: Immutable<Context>; }\n')


def check_projection():
    """The mirror against the Rust generator's cases. Returns the number checked."""
    document = json.loads(PROJECTION_CASES.read_text())
    def expand(schema):
        if isinstance(schema, str) and schema.startswith('DEEP-'):
            inner = {'type': 'string'}
            for _ in range(int(schema[5:])):
                inner = {'type': 'array', 'items': inner}
            return inner
        return schema
    for case in document['cases']:
        try:
            result, error = schema_type(expand(case['schema'])), None
        except ValueError as raised:
            result, error = None, str(raised)
        if 'error' in case:
            if error is None or case['error'] not in error:
                raise ValueError(f"projection case {case['name']!r}: expected error {case['error']!r}, got {result or error!r}")
        else:
            expected = case.get('rust', {}).get('type', case['type'])
            if result != expected:
                raise ValueError(f"projection case {case['name']!r}: expected {expected!r}, got {result or error!r}")
    for case in document['atoms']:
        if atom_declarations(case['schemas']) != case['declarations']:
            raise ValueError(f"atom declarations case {case['name']!r} differs")
    return len(document['cases']) + len(document['atoms'])


def check(path, input_schema, output_schema, registry, input_uri, output_uri):
    request, config = load(path)
    schemas = {'Input':inline(input_schema,registry.resolver(input_uri)), 'Output':inline(output_schema,registry.resolver(output_uri)), 'Context':{'type':'object','additionalProperties':False}, 'Case':{}}
    sidecars = list(path.parent.glob(path.name.removesuffix('.atom.astro') + '.context.schema.*'))
    if len(sidecars) > 1: raise ValueError(f'{path}: ambiguous context sidecar')
    if sidecars:
        import yaml
        schemas['Context'] = inline(yaml.safe_load(sidecars[0].read_text()), registry.resolver(sidecars[0].as_uri()))
    types = Path(request['root']) / '.atomic/types'
    supplied = {str(types / (path.name.removesuffix('.atom.astro') + '.d.ts')): atom_declarations(schemas), str(types / 'authoring.d.ts'): (ROOT / 'authoring/authoring.d.ts').read_text()}
    declarations = invoke(request | {'action':'check','declarations':supplied})
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
