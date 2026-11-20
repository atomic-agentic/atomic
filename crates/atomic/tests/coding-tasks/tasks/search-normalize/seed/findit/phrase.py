"""Phrase matching against term positions."""


def matches(index, doc_id, phrase):
    """True if the phrase's terms sit in the document at their offsets from some start position."""
    if not phrase:
        return False
    first_offset, first_term = phrase[0]
    rest = [(offset - first_offset, set(index.positions(term, doc_id))) for offset, term in phrase[1:]]
    for start in index.positions(first_term, doc_id):
        if all(start + delta in positions for delta, positions in rest):
            return True
    return False
