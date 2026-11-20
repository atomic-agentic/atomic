"""Splitting text into words: runs of letters and digits."""

import re
import unicodedata

_WORD = re.compile(r"[^\W_]+")


def words(text):
    """The words of text, in order.

    A combining mark is not a word character to the regex, so "e" + U+0301 would split in two.
    Composing first (NFC) keeps such a pair together wherever a composed form exists.
    """
    return _WORD.findall(unicodedata.normalize("NFC", text))
