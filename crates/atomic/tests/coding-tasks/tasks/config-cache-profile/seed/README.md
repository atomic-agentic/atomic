# pricer

A small command-line tool that turns catalog prices into customer quotes for each sales region. Each
region is a *profile* with its own currency, exchange rate, tax rate and bulk discount rule.

## Usage

```
python3 -m pricer quote --profile eu WIDGET-S 3     one quote
python3 -m pricer batch data/jobs-example.txt       one quote per line of a jobs file
python3 -m pricer profiles                          every profile and its label
```

Global options, before the command:

- `--config DIR`: the configuration directory (default: `$PRICER_CONFIG_DIR`, else `config`).
- `--catalog FILE`: the catalog (default: `data/catalog.csv`).

A jobs file has one job a line, `PROFILE SKU QUANTITY`. Blank lines and lines starting with `#` are
skipped. A line that cannot be priced is reported on stderr as `line N: message`, the other lines are
still priced, and the exit status is 1.

## Output

One line a quote:

```
eu WIDGET-S x3: net 26.36 EUR, tax 5.54 EUR, total 31.90 EUR
```

A quote that earned the bulk discount ends with ` (bulk -5%)`, with the profile's percentage.

## How a quote is priced

1. The catalog price (in USD) times the quantity is the base.
2. When the quantity is at least `bulk_threshold`, `bulk_discount_pct` percent of the base comes off.
3. What is left is converted with `fx_rate` and rounded, half up, to the currency's `decimals`. That is
   the net.
4. Tax is `tax_rate` percent of the net, rounded the same way. The total is net plus tax.

## Configuration

`config/defaults.ini` holds the defaults. `config/profiles/NAME.ini` overrides them for profile `NAME`.
Both have one `[pricing]` section with these keys: `label`, `currency`, `fx_rate`, `tax_rate`,
`decimals`, `bulk_threshold` and `bulk_discount_pct`.

An environment variable `PRICER_<PROFILE>_<KEY>`, in capitals, overrides one key for one profile, for
example `PRICER_EU_TAX_RATE=19`.

Loading a profile's settings parses its files, which is slow on the shared drive where production keeps
them, so settings are cached: a process loads each profile at most once.

## Tests

```
python3 -m unittest discover -s tests -t .
```
