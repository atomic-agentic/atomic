# findit

findit is a small full-text search over a folder of text files, for the team's
runbooks and notes. It builds an inverted index in memory, with term positions, and
answers queries with words, quoted phrases and exclusions.

    python3 -m findit --docs docs 'refund "point of sale" -draft'

prints one line per matching document, best first: its id (the file name without
`.txt`), a tab, and its score to four decimals. With no match it prints `no matches`
on stderr and exits 1. `--limit N` keeps the first N lines.

## How text becomes terms

Document text and query text go through the same steps, in this order:

1. **Words.** Text is split into words at every character that is not a letter or a
   digit. `state-of-the-art` is four words, `don't` is two (`don`, `t`).
2. **Folding.** Each word is decomposed (Unicode NFKD), combining marks are removed,
   and the result is case-folded (`str.casefold`). So `Café`, `CAFE` and `cafe` are
   one term, `résumé` and `Resume` are one term, and `Straße`, `STRASSE` and `strasse`
   are one term.
3. **Stopwords.** Folded words in the stopword list (`findit/stopwords.py`) are
   dropped.

What is left are the terms. A term's **position** is its place among the terms of its
document, counting from 0, so stopwords take no position. In "the state of the art",
`state` is at 0 and `art` at 1.

## Queries

- A **word** outside quotes: every term it gives must be in the document, anywhere.
  `state-of-the-art`, unquoted, needs `state` and `art`.
- A **"quoted phrase"**: its terms must appear in the document at consecutive
  positions, in order. Because stopwords take no position on either side, the phrase
  `"state of the art"` matches "state-of-the-art" and "state of the art", and also
  "state and art".
- A word starting with `-`, as in `-draft`: no document holding any term it gives is
  returned.
- Words that are only stopwords are ignored. A query with no terms to look for, only
  exclusions or stopwords, matches nothing.

Query text is folded exactly as document text is, so a query matches whatever case and
accents the documents use, and the other way round.

## Ranking

A document's score is the sum, over every term the query looks for (the terms of its
words, then the terms of its phrases; a term given twice counts twice), of

    (occurrences of the term in the document / terms in the document) * ln(1 + documents / documents holding the term)

Documents are sorted by score, highest first, and by id when scores are equal (to
nine decimals).

## Layout

- `findit/normalize.py`: folding.
- `findit/tokenize.py`: splitting text into words.
- `findit/stopwords.py`: the stopword list.
- `findit/analyzer.py`: text to terms with positions.
- `findit/index.py`: the inverted index.
- `findit/query.py`: parsing queries.
- `findit/phrase.py`: phrase matching.
- `findit/rank.py`: scores.
- `findit/search.py`: putting it together.
- `findit/corpus.py`, `findit/cli.py`: reading the folder, the command line.

## Tests

    python3 -m unittest discover -s tests -t .
