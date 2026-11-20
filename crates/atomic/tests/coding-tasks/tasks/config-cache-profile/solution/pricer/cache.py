"""Memoization for loaders that are slow and give the same answer for the same arguments.

Settings are parsed from several files, so they are loaded once per process and kept here. Tests call
clear_all() between cases, so that each one sees the files as they are.
"""

import functools
import threading

# Separates positional from keyword arguments in a key, so that f(1, b=2) and f(1, 2) stay apart.
_KWD_MARK = object()

_lock = threading.Lock()
_registry = []


def _make_key(args, kwargs):
    """A hashable key for one call."""
    key = tuple(args)
    if kwargs:
        key += (_KWD_MARK,) + tuple(sorted(kwargs.items()))
    return key


def memoize(fn):
    """Cache fn's results by its arguments, which must be hashable.

    The wrapper has cache_clear() and cache_info(), as functools.lru_cache does.
    """
    store = {}
    stats = {"hits": 0, "misses": 0}

    @functools.wraps(fn)
    def wrapper(*args, **kwargs):
        key = _make_key(args, kwargs)
        with _lock:
            if key in store:
                stats["hits"] += 1
                return store[key]
        # Not under the lock: a slow load must not hold up other callers.
        value = fn(*args, **kwargs)
        with _lock:
            stats["misses"] += 1
            return store.setdefault(key, value)

    def cache_clear():
        with _lock:
            store.clear()
            stats["hits"] = stats["misses"] = 0

    def cache_info():
        with _lock:
            return {"hits": stats["hits"], "misses": stats["misses"], "size": len(store)}

    wrapper.cache_clear = cache_clear
    wrapper.cache_info = cache_info
    _registry.append(wrapper)
    return wrapper


def clear_all():
    """Forget every cached result."""
    for wrapper in list(_registry):
        wrapper.cache_clear()
