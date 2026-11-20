# invoicing

A small invoicing tool for Lantern Row Workshop. It reads an invoice as JSON, checks it, works out the
amounts and prints the invoice as text, or its totals as JSON.

```
python3 -m invoicing render examples/inv-2207.json    # the invoice, as text
python3 -m invoicing totals examples/inv-2207.json    # its amounts, as JSON
python3 -m invoicing check examples/*.json            # check invoices without printing them
```

Exit codes: 0 when every file is a valid invoice, 1 when one is not (the reason goes to stderr), 2 for
bad usage.

Run the tests with:

```
python3 -m unittest discover -s tests -t .
```

## Invoice files

```json
{
  "number": "INV-2207",
  "date": "2026-10-03",
  "currency": "EUR",
  "customer": {"id": "C-118", "name": "Halvard Joinery"},
  "lines": [
    {"sku": "OAK-12", "description": "Oak dowel, 12 mm", "quantity": "3",
     "unit_price": "4.115", "discount": "0", "tax": "std"}
  ],
  "notes": "Payable within 30 days."
}
```

- `quantity`, `unit_price` and `discount` are strings holding plain decimal numbers, such as `"3"`,
  `"1.5"` or `"0.0125"`. No exponents, no thousands separators. None may be negative.
- `discount` is a percentage from 0 to 100, and defaults to `"0"`.
- `tax` is one of the codes below, and defaults to `"std"`.
- `notes` is optional.

## Tax codes

| Code      | Rate  |
|-----------|-------|
| `std`     | 20%   |
| `low`     | 12.5% |
| `reduced` | 5.5%  |
| `zero`    | 0%    |

## Money rules

These are the rules the accountants agreed. The printed invoice must follow them exactly, for every
invoice, because customers check the arithmetic.

1. All amounts are worked out exactly, in decimal, and are only ever rounded to the cent, as the rules
   below say. Rounding is **half up**: a third decimal of 5 or more rounds up, so 2.675 is 2.68 and
   0.005 is 0.01.
2. A line's amount is its quantity times its unit price, less its discount, rounded to the cent.
3. The subtotal is the sum of the rounded line amounts, so the printed lines always add up to it.
4. The tax for each code is its rate times the sum of the rounded line amounts with that code,
   rounded to the cent. Taxes are listed in the order of the table above, one for each code the
   invoice uses, even when the tax is 0.00.
5. The total is the subtotal plus the rounded taxes, so the printed footer always adds up too.

## The printed invoice

```
INVOICE INV-2207                                              2026-10-03
Customer: Halvard Joinery (C-118)

SKU        Description                  Qty       Unit   Disc     Amount
------------------------------------------------------------------------
OAK-12     Oak dowel, 12 mm               3      4.115             12.35
...
------------------------------------------------------------------------
                                            Subtotal                 ...
                                            Tax std 20%              ...
                                            Tax reduced 5.5%         ...
                                            Total EUR                ...

Payable within 30 days.
```

Amounts have two decimals and a comma between thousands. Unit prices show two to four decimals.

`totals` prints the same amounts as JSON, without thousands separators:

```json
{"number": "INV-2207", "lines": ["12.35", "..."], "subtotal": "...", "taxes": {"std": "..."}, "total": "..."}
```

## Layout

- `invoicing/money.py`: parsing, rounding and formatting amounts.
- `invoicing/model.py`: the invoice as plain data.
- `invoicing/loader.py`: reading and checking invoice files.
- `invoicing/taxes.py`: the tax codes.
- `invoicing/pricing.py`: the amount of one line.
- `invoicing/totals.py`: subtotal, taxes and total.
- `invoicing/render.py`: the printed invoice.
- `invoicing/cli.py`: the command line.
