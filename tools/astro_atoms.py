# SPDX-License-Identifier: Apache-2.0
"""Use the shipped Astro compiler for catalog checks; never execute atom modules.

The mirror asks the same compiler child atomic asks: atomic's own binary, a
cleared environment, the atom sources in the request, and the same bounds on the
exchange (`invoke`). It never runs atom code itself. A captured TypeScript
helper runs only inside atomic, in its helper runtime, so an operation that asks
for a helper call is refused naming that helper (`helper_refusal`), and the one
catalog operation that needs a rendered prompt from a helper-backed atom -- a
fixture pair -- asks atomic for its own preview render (`preview`), which is a
preview and not a model call.
"""
from pathlib import Path
import json
import os
import queue
import selectors
import shutil
import signal
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
CACHE = {}
SCHEMA_DECLARATION_CACHE = {}

# The bounds of one compiler exchange, as atomic keeps them
# (crates/atomic-core/src/compiler_process.rs): the parent's time limit, one
# line's size, everything one exchange may write, and the stderr kept.
COMPILER_SECONDS = 35
MESSAGE_LIMIT = 8_388_608
RESPONSE_LIMIT = 4 * MESSAGE_LIMIT
STDERR_LIMIT = 65_536
# A preview is a whole atomic process: it loads the atom, runs its helper calls
# and renders, so it gets a longer bound than one compiler exchange.
PREVIEW_SECONDS = 120
PREVIEW_STDOUT_LIMIT = MESSAGE_LIMIT
PREVIEW_STDERR_LIMIT = STDERR_LIMIT
# Marks a response line that holds no `call` key, so it is the final answer. A
# line that does hold one is a helper-call request whatever it holds, as it is
# for atomic's reader.
_MISSING = object()


def compiler():
    """The atomic binary whose embedded compiler runs the checks: ATOMIC_BIN, else this checkout's debug build."""
    binary = Path(os.environ.get('ATOMIC_BIN') or ROOT / 'target/debug/atomic').resolve()
    if not binary.is_file():
        raise ValueError(f'Astro checks run on the compiler embedded in atomic, and {binary} is not built; run cargo build or set ATOMIC_BIN')
    return binary


def _refusal(call, captures, action):
    """One answer to a helper-call request, which is always a refusal.

    The shape is production's (`{"error":{"kind","message"}}`, see
    `astro::helper_call`), and so is the order the two structural refusals are
    asked in -- the capture must name a helper the atom captured, then the export
    must name one -- so the worker reports the refusal as the failure of the call
    it tried to make, naming the helper. The refusals are told apart because they
    are different faults: a call naming a capture the atom never loaded is a
    damaged or foreign atom, and one naming a capture it did load is this mirror
    being where a helper cannot run.
    """
    digest = call.get('capture') if isinstance(call, dict) else None
    if captures is None or not isinstance(digest, str) or digest not in captures:
        return {'error': {'kind': 'refused', 'message': "the helper is not in the atom's captured helpers"}}
    export = call.get('export')
    if not isinstance(export, str) or not export:
        return {'error': {'kind': 'refused', 'message': 'a helper call names its export'}}
    return {'error': {'kind': 'refused', 'message':
                      f'the catalog mirror does not run captured helpers, so the {action} of a helper-backed atom cannot be checked here; atomic answers this call in its own helper runtime'}}


def helper_refusal(captures=(), action='render'):
    """Refuse every helper call the worker asks this mirror to run."""
    return lambda call: _refusal(call, frozenset(captures), action)


def _no_helper(call):
    """Refuse a helper call from an operation that never got as far as loading captures."""
    return _refusal(call, None, 'operation')


def invoke(request, answer=None):
    """Run one compiler operation, answering the worker's helper-call requests.

    As in atomic itself: the compiler is atomic run as its own child, with a
    cleared environment, and the atom sources travel in the request. While it
    works the worker may ask the host to run a captured helper: one `call` line
    on its stdout, one line back on its stdin, and stdin open until the final
    answer, so it can read each reply (`compiler_process::run_interactive`).
    `answer(call)` answers those calls; with none, every call is refused. A
    refusal is an answer, not a fallback: the worker fails the operation naming
    the helper, so an atom that needs a helper never renders something else.
    The exchange keeps atomic's own bounds -- the time limit, a session of its
    own, per-line and total output limits, the stderr kept -- and the child is
    killed and reaped on every path, including a refused call and a timeout.
    """
    binary = str(compiler())
    request_line = json.dumps(request)
    try:
        child = subprocess.Popen([binary, '__atomic-compiler'], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, env={}, start_new_session=True)
    except OSError as failed:
        raise ValueError(f'cannot start the Astro compiler with {binary}: {failed}')
    outgoing, lines, errors = queue.Queue(), queue.Queue(), queue.Queue()

    def write_stdin():
        # Lines go out in order on their own thread, so a worker that stops
        # reading cannot hold this process: the write fails once the child is
        # gone. stdin stays open until the answer, which is what lets the
        # worker read a reply at all.
        with child.stdin:
            for line in iter(outgoing.get, None):
                try:
                    child.stdin.write(line.encode('utf-8'))
                    child.stdin.write(b'\n')
                    child.stdin.flush()
                except (BrokenPipeError, OSError):
                    return

    def read_stdout():
        # Every line is bounded on its own and in total; a worker that stops
        # answering cannot make this grow without limit.
        total, pending = 0, bytearray()
        with child.stdout:
            while True:
                try:
                    chunk = child.stdout.readline(65_536)
                except OSError as failed:
                    lines.put(('error', f'running the Astro compiler: {failed}'))
                    outgoing.put(None)
                    return
                if not chunk:
                    if pending:
                        # A last line with no newline is an answer all the same,
                        # as it is for atomic's reader.
                        lines.put(('line', bytes(pending).decode('utf-8', 'replace')))
                    outgoing.put(None)
                    return
                total += len(chunk)
                pending += chunk
                if len(pending) > MESSAGE_LIMIT or total > RESPONSE_LIMIT:
                    lines.put(('error', 'compiler response limit exceeded'))
                    outgoing.put(None)
                    return
                if pending.endswith(b'\n'):
                    lines.put(('line', bytes(pending[:-1]).decode('utf-8', 'replace')))
                    pending.clear()

    def read_stderr():
        # A worker that fills stderr while its stdout is unread would block
        # both readers, so this drains it and keeps only what atomic keeps.
        kept = bytearray()
        with child.stderr:
            while True:
                try:
                    chunk = child.stderr.read(65_536)
                except OSError:
                    break
                if not chunk:
                    break
                if len(kept) < STDERR_LIMIT:
                    kept += chunk[:STDERR_LIMIT - len(kept)]
        errors.put(bytes(kept))

    for target in (write_stdin, read_stdout, read_stderr):
        threading.Thread(target=target, daemon=True).start()
    deadline = time.monotonic() + COMPILER_SECONDS
    answer_line, failure = None, None
    try:
        outgoing.put(request_line)
        while answer_line is None and failure is None:
            left = deadline - time.monotonic()
            if left <= 0:
                failure = f'the Astro compiler exceeded {COMPILER_SECONDS} seconds'
                break
            try:
                kind, value = lines.get(timeout=min(left, 0.25))
            except queue.Empty:
                continue
            if kind == 'error':
                failure = value
                break
            try:
                document = json.loads(value)
            except ValueError:
                failure = 'invalid compiler response'
                break
            call = document.get('call') if isinstance(document, dict) and 'call' in document else _MISSING
            if call is _MISSING:
                answer_line = value
                break
            reply = (answer or _no_helper)(call)
            outgoing.put(json.dumps(reply))
    finally:
        # The writer stops at the closed queue and the reader at the closed
        # pipe, so nothing here can outlive the child. A child that has already
        # answered is waited for rather than killed, as atomic waits for it:
        # only one that outlives the exchange is stopped, and then reaped.
        outgoing.put(None)
        grace = max(0.0, min(deadline - time.monotonic(), 5.0)) or 0.05
        try:
            child.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
    if failure is None:
        failure = _finished(child, errors)
    if failure:
        raise ValueError(failure)
    if answer_line is None:
        raise ValueError('missing compiler result')
    response = json.loads(answer_line)
    if 'error' in response:
        error = response['error']
        if isinstance(error, dict):
            reports = [f"{d['file']}:{d['line']}:{d['column']}: {d['message']}" for d in error.get('diagnostics', [])]
            where = ':'.join(str(part) for part in (error.get('file'), error.get('line')) if part)
            raise ValueError('\n'.join(reports) or (f'{where}: ' if where else '') + error.get('message', 'compiler error'))
        raise ValueError(error)
    return response['ok']


def _finished(child, errors):
    """Why a compiler child that is gone failed, or nothing when it answered."""
    try:
        stderr = errors.get(timeout=5)
    except queue.Empty:
        stderr = b''
    if child.returncode:
        note = stderr.decode('utf-8', 'replace').strip()
        return f'the Astro compiler crashed: {note}' if note else f'the Astro compiler crashed: exit status {child.returncode}'
    return None


def read_source(target, root):
    """Read an Astro component or captured TypeScript helper as atomic does."""
    canonical = Path(target).resolve(strict=True)
    helper = canonical.suffix == '.ts' and not canonical.name.endswith('.d.ts')
    if not canonical.is_relative_to(root) or not (canonical.suffix == '.astro' or helper):
        raise ValueError(f'{target}: an imported source must be an Astro component or a .ts helper inside the project root')
    with canonical.open('rb') as source_file:
        source = source_file.read(262_145)
    if len(source) > 262_144:
        raise ValueError(f'{target}: source size limit exceeded')
    return str(canonical), source.decode('utf-8')


def load(path):
    """The compiler's frozen request for one atom, and its frontmatter config."""
    request, config, _ = _load(path)
    return request, config


def _load(path):
    """As `load`, with the digests of the helper modules the atom captured."""
    path = Path(path).resolve()
    if path in CACHE:
        return CACHE[path]
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
        if sum(len(source.encode('utf-8')) for source in request['sources'].values()) > 1_048_576 or len(request['sources']) > 128:
            raise ValueError(f'{path}: component graph limit exceeded')
        result = invoke(request)
        if 'needsSources' not in result:
            break
        targets = result['needsSources']
    else:
        raise ValueError(f'{path}: component graph limit exceeded')
    CACHE[path] = (request | {'sources': result['sources'], 'sourcePaths': result['sourcePaths']},
                   result['config'], sorted(result.get('helpers') or {}))
    return CACHE[path]


def helpers(path):
    """The digests of the helper module graphs this atom captured when it loaded.

    An atom with none renders in the compiler child alone. An atom with any can
    only render through atomic itself, because answering its helper calls means
    running them (`preview`).
    """
    return _load(path)[2]


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
    """One atom's declarations, byte for byte as atomic's generator writes them.

    `Context` appears only when the atom declares a context schema, so an atom
    that declares none has no `context` in `Props` or `Config` to read.
    """
    types = '\n'.join(f"export type {'ContextData' if key == 'Context' else key} = {schema_type(schema)};" for key, schema in schemas.items())
    declared = 'Context' in schemas
    return ('// Generated from the full runtime contracts. Do not edit.\n'
            "import type { AtomConfig } from './authoring';\n" + types + '\n'
            + ('export type Context = Immutable<ContextData>;\n' if declared else '')
            + 'export type Reply = Output;\n'
            'export type EvalCase = Case;\n'
            + f"export type Config = AtomConfig<Input, Reply, EvalCase, Judges{', Context' if declared else ''}>;\n"
            + 'export type Immutable<T> = T extends object ? { readonly [K in keyof T]: Immutable<T[K]> } : T;\n'
            + ('export interface Props { input: Input; readonly context: Immutable<Context>; }\n' if declared
               else 'export interface Props { input: Input; }\n'))


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


def context_sidecar(path):
    """The atom's context sidecar, or None. Two of them is refused, as atomic refuses it."""
    sidecars = list(path.parent.glob(path.name.removesuffix('.atom.astro') + '.context.schema.*'))
    if len(sidecars) > 1:
        raise ValueError(f'{path}: ambiguous context sidecar')
    return sidecars[0] if sidecars else None


def declared_context(path, config, registry):
    """The public context schema an atom declares, or None when it declares none.

    None is not the same as a schema that declares no properties: an atom with
    no context schema has no `context` to read, in the template or in a
    callback, and needs no snapshot. `context: null` is how the authoring
    contract spells "this atom writes no schema", so the key is ignored and the
    sidecar beside the atom is what declares one — as in atomic's own loader.
    """
    schema = config.get('context')
    if schema is not None:
        return inline(schema, registry.resolver(path.as_uri()))
    sidecar = context_sidecar(path)
    if sidecar is None:
        return None
    import yaml
    return inline(yaml.safe_load(sidecar.read_text()), registry.resolver(sidecar.as_uri()))


def declared_judges(path, config, registry):
    """The `Judges` contract atomic projects, so a scorer's `judge` is typed."""
    properties = {}
    for name, judge in (config.get('judges') or {}).items():
        resolver = registry.resolver(path.as_uri())
        properties[name] = {
            'type': 'object', 'additionalProperties': False, 'required': ['input', 'output'],
            'properties': {'input': inline((judge or {}).get('input'), resolver),
                           'output': inline((judge or {}).get('output'), resolver)},
        }
    return {'type': 'object', 'additionalProperties': False,
            'required': list(properties), 'properties': properties}


def shared_schema_declarations(request):
    """Ask atomic for schema modules in a disposable copy of this project.

    The catalog mirror projects an atom's own types, but shared-schema module
    names and source annotations belong to atomic's generator. Supplying its
    result as virtual declarations keeps this check independent of stale editor
    files and never installs npm packages or writes state into the project.
    """
    root = Path(request['root'])
    if not any('.atomic/types/schemas/' in source for source in request['sources'].values()):
        return {}
    if root in SCHEMA_DECLARATION_CACHE:
        return SCHEMA_DECLARATION_CACHE[root]
    binary = compiler()
    with tempfile.TemporaryDirectory(prefix='atomic-catalog-schemas-') as scratch_name:
        scratch = Path(scratch_name)
        copied = scratch / 'project'
        def omit_generated(directory, names):
            if Path(directory).name == '.atomic':
                # An embedded project keeps its manifest under .atomic/.
                return {'types', 'cache', 'state'} & set(names)
            omitted = {'node_modules', 'target', '.git', '.beads'} & set(names)
            # A manifest-backed project has no source under .atomic, so none
            # of its stale editor declarations enters the fresh copy. An
            # embedded project is the exception: its manifest and atom sources
            # live there, while the branch above omits generated directories.
            if not (Path(directory) / '.atomic/atomic.yaml').is_file():
                omitted |= {'.atomic'} & set(names)
            return omitted

        shutil.copytree(root, copied, ignore=omit_generated)
        environment = preview_environment(binary, scratch)
        checked = subprocess.run([str(binary), 'check', '--json'], cwd=copied,
                                 env=environment, text=True, capture_output=True,
                                 timeout=PREVIEW_SECONDS)
        if checked.returncode:
            raise ValueError(f'{root}: atomic could not generate shared schema declarations: {checked.stdout.strip() or checked.stderr.strip()}')
        generated = copied / '.atomic/types/schemas'
        declarations = {
            str(root / '.atomic/types/schemas' / path.relative_to(generated)): path.read_text()
            for path in generated.rglob('*.d.ts')
        } if generated.is_dir() else {}
    if not declarations:
        raise ValueError(f'{root}: atomic generated no shared schema declarations')
    SCHEMA_DECLARATION_CACHE[root] = declarations
    return declarations


def check(path, input_schema, output_schema, registry, input_uri, output_uri,
          provision_shared_schemas=True):
    request, config, captures = _load(path)
    schemas = {'Input':inline(input_schema,registry.resolver(input_uri)), 'Output':inline(output_schema,registry.resolver(output_uri))}
    context = declared_context(path, config, registry)
    if context is not None:
        schemas['Context'] = context
    schemas['Case'] = inline(config.get('case', True), registry.resolver(path.as_uri()))
    schemas['Judges'] = declared_judges(path, config, registry)
    types = Path(request['root']) / '.atomic/types'
    supplied = {str(types / (path.name.removesuffix('.atom.astro') + '.d.ts')): atom_declarations(schemas), str(types / 'authoring.d.ts'): (ROOT / 'authoring/authoring.d.ts').read_text()}
    if provision_shared_schemas:
        supplied.update(shared_schema_declarations(request))
    # The worker consumes these as virtual declarations. Its returned map is
    # editor output, not needed by render or validate, so leave the source tree
    # untouched. The fresh generator copy above is deleted on return.
    invoke(request | {'action':'check','declarations':supplied,
                      'contextDeclared': context is not None},
           helper_refusal(captures, 'check'))
    return config


def preview_environment(binary, home):
    """The whole environment a preview render may have, and nothing else.

    Nothing here is inherited: the binary's directory on `PATH`, a home and an
    atomic home under `home` (so its state is this run's, not the person's and
    not the example's), and host detection pointed at loopback, so a preview
    cannot fall through to a provider. `render` is a preview and calls no model;
    this is what keeps even its detection offline.
    """
    return {
        'PATH': os.pathsep.join([str(Path(binary).parent), os.defpath]),
        'HOME': str(Path(home) / 'home'),
        'ATOMIC_HOME': str(Path(home) / 'home/.local/share/atomic'),
        'ATOMIC_DETECT_OLLAMA_ADDR': '127.0.0.1:9',
    }


def _run_preview(command, root, environment):
    """Collect a bounded preview response and stop its whole process group on failure."""
    try:
        child = subprocess.Popen(command, cwd=str(root), env=environment,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 start_new_session=True)
    except OSError as failed:
        raise ValueError(f'cannot start the preview render with {command[0]}: {failed}')
    output = {child.stdout: bytearray(), child.stderr: bytearray()}
    limits = {child.stdout: PREVIEW_STDOUT_LIMIT, child.stderr: PREVIEW_STDERR_LIMIT}
    deadline = time.monotonic() + PREVIEW_SECONDS
    completed = False
    try:
        with selectors.DefaultSelector() as selector:
            for stream in output:
                selector.register(stream, selectors.EVENT_READ)
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise ValueError(f'the preview render exceeded {PREVIEW_SECONDS} seconds')
                ready = selector.select(remaining)
                if not ready:
                    raise ValueError(f'the preview render exceeded {PREVIEW_SECONDS} seconds')
                for key, _ in ready:
                    stream = key.fileobj
                    chunk = os.read(stream.fileno(), 65_536)
                    if not chunk:
                        selector.unregister(stream)
                        continue
                    if len(output[stream]) + len(chunk) > limits[stream]:
                        name = 'stdout' if stream is child.stdout else 'stderr'
                        raise ValueError(f'the preview render exceeded its {name} limit')
                    output[stream].extend(chunk)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ValueError(f'the preview render exceeded {PREVIEW_SECONDS} seconds')
        child.wait(timeout=remaining)
        completed = True
        return subprocess.CompletedProcess(command, child.returncode,
                                           output[child.stdout].decode('utf-8', 'replace'),
                                           output[child.stderr].decode('utf-8', 'replace'))
    except subprocess.TimeoutExpired:
        raise ValueError(f'the preview render exceeded {PREVIEW_SECONDS} seconds')
    finally:
        # The child owns a new session. Its helpers can outlive the CLI itself,
        # so failure must stop the group before waiting for the CLI process.
        if not completed:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        child.wait()
        child.stdout.close()
        child.stderr.close()


def preview(path, input_value, home):
    """Render one atom's prompt with atomic itself: a preview, not a model call.

    `atomic render --preview-helpers` loads the atom, answers each captured helper call in
    atomic's own helper runtime -- the fresh capability-free isolate per call,
    its limits and its refusals, which is exactly what this mirror must never
    supply -- and prints the two turns as JSON. This explicit preview is for
    catalog fixture checks; its fresh clock does not claim byte parity with a
    durable run. It needs no model and no network.

    The child gets an environment of its own and nothing else: a home and an
    atomic home under `home`, so its state goes there and not into the example
    or a person's own, and host detection pointed at loopback, so nothing it
    does can reach a provider. `home` is the caller's private directory, made
    0700 by `tempfile`; the input file goes in a directory of this call's own
    under it, which is removed whatever the render does. It runs in the atom's
    own project and reads the sources the compiler just checked; `render` writes
    nothing there -- only `atomic check` regenerates declarations.
    """
    binary = compiler()
    path = Path(path).resolve()
    root = Path(_load(path)[0]['root'])
    home = Path(home)
    if not home.is_dir():
        raise ValueError(f'{home}: the preview home is not a directory; make it private (0700) before rendering')
    environment = preview_environment(binary, home)
    # mkdtemp makes this directory of ours alone: 0700, unnamed in advance, so
    # nothing can be planted at its name before the input is written there.
    scratch = Path(tempfile.mkdtemp(prefix='input-', dir=home))
    try:
        given = scratch / 'input.json'
        given.write_text(json.dumps(input_value), encoding='utf-8')
        started = _run_preview([str(binary), '--json', 'render', str(path), '--input', str(given),
                                '--preview-helpers'],
                               root, environment)
    finally:
        # The input holds the fixture's text, so it goes whether the render
        # answered, failed or could not be started.
        shutil.rmtree(scratch)
    if started.returncode:
        detail = started.stdout.strip()
        if detail.startswith('{'):
            # `--json` reports a failure as `{"error":{kind,description,action}}`
            # (`failure::document`), so the words to show are under `error`.
            try:
                error = json.loads(detail).get('error')
            except ValueError:
                error = None
            named = (error.get(part) for part in ('description', 'action')) if isinstance(error, dict) else ()
            detail = ': '.join(part for part in named if isinstance(part, str) and part) or detail
        said = detail or started.stderr.strip()
        raise ValueError(f'{path}: the preview render failed: {said or f"exit status {started.returncode}"}')
    try:
        document = json.loads(started.stdout)
    except ValueError as failed:
        raise ValueError(f'{path}: the preview render printed no JSON: {failed}')
    if not isinstance(document, dict) or not all(key in document for key in ('developer', 'user')):
        raise ValueError(f'{path}: the preview render printed no turns: {started.stdout[:200]}')
    return document


def fixture(path, input_value, output_value):
    """Check one fixture pair: render the atom with this input, validate that output.

    The render goes to atomic itself when the atom captured helpers, and to the
    compiler child when it did not (`preview`); the golden output is always
    checked by the shipped worker's interpreted validators. Returns the rendered
    prompt: `developer`, `user`, and on the compiler path the `bindings` it made.
    """
    request, config, captures = _load(path)
    options = {'runId': 'catalog', 'origin': 'external'}
    settings = Path(request['root']) / 'context.settings.json'
    # The same contract atomic's loader applies: a schema written in the field,
    # else the sidecar beside the atom, else no public context at all. Running a
    # contextual atom with no snapshot would leave its validators reading an
    # absent `context`, so the check says so instead of passing, and an atom
    # that declares no context is run with no snapshot at all.
    schema = config.get('context')
    if schema is None:
        sidecar = context_sidecar(path)
        if sidecar is not None:
            import yaml
            schema = yaml.safe_load(sidecar.read_text())
    declared = schema is not None
    if declared and captures:
        # The CLI preview that answers captured helper calls has no context
        # snapshot. Refuse this combination before it can render with a missing
        # public context; settings alone do not make a context-free atom need one.
        raise ValueError(f'{path}: a contextual atom that captures helpers is outside the catalog mirror: the preview render that answers helper calls carries no context snapshot')
    if declared:
        if not settings.is_file():
            raise ValueError(f"{path}: its atom declares a public context, so running it needs context.settings.json beside the project manifest")
        host = json.loads((settings.parent / 'context.fixture.json').read_text())
        snapshot = invoke(request | {'action': 'resolveContext', 'programHash': 'catalog',
                                     'environment': {item['name']: host['environment'][item['name']] for item in json.loads(settings.read_text())['environment'].values() if item['name'] in host['environment']},
                                     'resolution': {'document': {'$schema': 'https://json-schema.org/draft/2020-12/schema', '$defs': {'RuntimeContext': schema}},
                                                    'settings': json.loads(settings.read_text()),
                                                    'profile': host['profile'], 'metadata': host['metadata']}})
        options = {'runId': snapshot['runId'], 'origin': 'external', 'contextSnapshot': snapshot}
    if captures:
        with tempfile.TemporaryDirectory(prefix='atomic-catalog-preview-') as directory:
            rendered = preview(path, input_value, Path(directory))
    else:
        rendered = invoke(request | {'action': 'render', 'input': input_value,
                                     'options': options, 'contextDeclared': declared},
                          helper_refusal(captures, 'render'))
    result = invoke(request | {'action': 'validate',
                               'values': {'input': input_value, 'output': output_value},
                               'options': options, 'contextDeclared': declared},
                    helper_refusal(captures, 'validator'))
    if result.get('valid') is not True:
        raise ValueError(f'{path}: golden output fails interpreted validators: {result}')
    return rendered
