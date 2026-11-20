"""The checks `deployctl check` runs on an expanded config. Each rule yields (section, key, message)."""

import re

ENVIRONMENTS = ("dev", "staging", "prod")
DURATION = re.compile(r"^[1-9][0-9]*(ms|s|m|h)$")
NAME = re.compile(r"^[a-z][a-z0-9-]{1,38}[a-z0-9]$")


def required(values):
    for section, key in (("service", "name"), ("service", "image"), ("deploy", "env"), ("deploy", "replicas")):
        if key not in values.get(section, {}):
            yield section, None, f"[{section}] needs {key}"


def service_name(values):
    name = values.get("service", {}).get("name")
    if name is not None and not NAME.match(name):
        yield "service", "name", f"bad service name {name!r}: 3-40 of a-z, 0-9 and -, starting with a letter"


def environment(values):
    env = values.get("deploy", {}).get("env")
    if env is not None and env not in ENVIRONMENTS:
        yield "deploy", "env", f"env must be one of {', '.join(ENVIRONMENTS)}, not {env!r}"


def replicas(values):
    text = values.get("deploy", {}).get("replicas")
    if text is None:
        return
    if not text.isdigit() or not 1 <= int(text) <= 50:
        yield "deploy", "replicas", f"replicas must be a whole number from 1 to 50, not {text!r}"
    elif values.get("deploy", {}).get("env") == "prod" and int(text) < 2:
        yield "deploy", "replicas", "prod needs at least 2 replicas"


def ports(values):
    for section, settings in values.items():
        for key, text in settings.items():
            if key == "port" or key.endswith("_port"):
                if not text.isdigit() or not 1 <= int(text) <= 65535:
                    yield section, key, f"{key} must be a port from 1 to 65535, not {text!r}"


def image_tag(values):
    image = values.get("service", {}).get("image")
    if image is None:
        return
    name, _, tag = image.rpartition(":")
    if not name or "/" in tag:
        yield "service", "image", f"image {image!r} has no tag"
    elif tag == "latest" and values.get("deploy", {}).get("env") == "prod":
        yield "service", "image", "prod may not deploy the latest tag"


def health(values):
    settings = values.get("health")
    if settings is None:
        return
    path = settings.get("path", "")
    if not path.startswith("/"):
        yield "health", "path", f"health path must start with /, not {path!r}"
    for key in ("interval", "timeout"):
        if key in settings and not DURATION.match(settings[key]):
            yield "health", key, f"{key} must be a duration such as 10s or 2m, not {settings[key]!r}"


RULES = (required, service_name, environment, replicas, ports, image_tag, health)


def run(values):
    for rule in RULES:
        yield from rule(values)
