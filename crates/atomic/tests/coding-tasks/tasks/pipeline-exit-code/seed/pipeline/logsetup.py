"""Logging for the pipeline stages.

Every stage logs through a child of the "pipeline" logger, such as "pipeline.validate", and the shared
modules through their own children, such as "pipeline.csvio". configure() sends all of those records to
stderr in one format, and counts the errors, so that a stage can exit 1 when it logged any.
"""

import logging
import sys

ROOT = "pipeline"
FORMAT = "%(levelname)s %(name)s: %(message)s"


class ErrorCounter(logging.Handler):
    """Counts the records at ERROR or above that reach it. It writes nothing."""

    def __init__(self):
        super().__init__(level=logging.ERROR)
        self.count = 0

    def emit(self, record):
        self.count += 1


def stage_logger(stage):
    """The logger a stage logs through."""
    return logging.getLogger(f"{ROOT}.{stage}")


def configure(stage, verbose=False, stream=None):
    """Set up logging for one stage, and return the handler that counts its errors.

    Calling it again, as the tests do, replaces the handlers of the last call.
    """
    root = logging.getLogger(ROOT)
    for handler in list(root.handlers):
        root.removeHandler(handler)
    console = logging.StreamHandler(stream if stream is not None else sys.stderr)
    console.setFormatter(logging.Formatter(FORMAT))
    root.addHandler(console)
    root.setLevel(logging.DEBUG if verbose else logging.INFO)
    # Records stop here: the stages' output must not depend on whatever configured the root logger.
    root.propagate = False

    counter = ErrorCounter()
    errors = logging.getLogger(__name__)
    for handler in list(errors.handlers):
        if isinstance(handler, ErrorCounter):
            errors.removeHandler(handler)
    errors.addHandler(counter)

    stage_logger(stage).debug("logging configured for %s", stage)
    return counter
