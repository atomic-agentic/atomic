"""logship: format log records as syslog-style lines and ship them to a collector."""

from logship.config import Config, ConfigError, load
from logship.formatter import Formatter
from logship.record import Record

__all__ = ["Config", "ConfigError", "Formatter", "Record", "load"]
