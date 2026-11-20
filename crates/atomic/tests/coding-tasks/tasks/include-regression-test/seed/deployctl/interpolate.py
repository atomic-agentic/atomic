"""Expanding ${name} and ${section.name} in values, after every include is merged."""

import re

REFERENCE = re.compile(r"\$\$|\$\{([a-z0-9_.-]*)\}")


class InterpolationError(Exception):
    def __init__(self, value, message):
        super().__init__(message)
        self.value = value
        self.message = message


def expand(config):
    """section -> key -> expanded text, and a list of (Value, message) for references that cannot be expanded."""
    done = {}
    problems = []
    for section, values in config.sections.items():
        done[section] = {}
        for key, value in values.items():
            try:
                done[section][key] = _expand(config, section, key, chain=())
            except InterpolationError as error:
                problems.append((error.value, error.message))
                done[section][key] = value.text
    return done, problems


def _expand(config, section, key, chain):
    value = config.value(section, key)
    here = f"{section}.{key}"
    if here in chain:
        raise InterpolationError(value, "variable cycle: " + " -> ".join(chain + (here,)))
    chain = chain + (here,)

    def replace(match):
        if match.group(0) == "$$":
            return "$"
        name = match.group(1)
        if not name:
            raise InterpolationError(value, "empty variable ${}")
        ref_section, _, ref_key = name.rpartition(".")
        ref_section = ref_section or section
        if config.value(ref_section, ref_key) is None:
            raise InterpolationError(value, f"undefined variable ${{{name}}}")
        return _expand(config, ref_section, ref_key, chain)

    return REFERENCE.sub(replace, value.text)
