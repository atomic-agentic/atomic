"""Folded words that carry no meaning for search."""

STOPWORDS = frozenset(
    """
    a an and are as at be by for from has have in is it its of on or that the this to was were will with
    de la le les der die das und
    """.split()
)


def is_stopword(term):
    return term in STOPWORDS
