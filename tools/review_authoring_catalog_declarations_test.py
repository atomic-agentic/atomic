#!/usr/bin/env python3
"""Negative control for catalog shared-schema declaration provisioning."""
from pathlib import Path
import shutil
import tempfile

import yaml
from referencing import Registry, Resource

import astro_atoms

SOURCE = Path(__file__).resolve().parents[1] / 'examples/14-review-authoring'


def main():
    with tempfile.TemporaryDirectory(prefix='atomic-catalog-declaration-control-') as tmp:
        project = Path(tmp) / 'project'
        shutil.copytree(SOURCE, project, ignore=shutil.ignore_patterns('.atomic', 'node_modules'))
        assert not (project / '.atomic/types').exists()
        schema_files = list(project.rglob('*.schema.yaml'))
        registry = Registry()
        for schema_file in schema_files:
            registry = registry.with_resource(
                schema_file.as_uri(), Resource.from_contents(yaml.safe_load(schema_file.read_text())))
        atom = project / 'atoms/self-review.atom.astro'
        input_schema = atom.with_name('self-review.input.schema.yaml')
        output_schema = atom.with_name('self-review.output.schema.yaml')
        args = (atom, yaml.safe_load(input_schema.read_text()),
                yaml.safe_load(output_schema.read_text()), registry,
                input_schema.as_uri(), output_schema.as_uri())
        try:
            astro_atoms.check(*args, provision_shared_schemas=False)
        except ValueError as error:
            assert 'TS2307' in str(error) and 'review-packet' in str(error), error
        else:
            raise AssertionError('without shared declarations, TS2307 must return')
        assert not (project / '.atomic/types').exists()
        astro_atoms.check(*args)
        assert not (project / '.atomic/types').exists(), 'catalog check wrote editor files into source'
        stale = project / '.atomic/types/schemas/review-packet.d.ts'
        stale.parent.mkdir(parents=True)
        stale.write_text('export type ReviewPacket = never;\n')
        astro_atoms.check(*args)
        assert stale.read_text() == 'export type ReviewPacket = never;\n', 'catalog check changed a pre-existing editor file'
        assert list((project / '.atomic/types').rglob('*.d.ts')) == [stale], 'catalog check wrote more editor files'
    print('catalog shared-schema declarations: TS2307 negative control and provisioned check pass')


if __name__ == '__main__':
    main()
