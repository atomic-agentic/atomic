"""The product catalog: a CSV file of sku, name and price in USD."""

import csv
from dataclasses import dataclass

from .money import parse_price


class CatalogError(Exception):
    """The catalog is unreadable, or has no such product."""


@dataclass(frozen=True)
class Item:
    sku: str
    name: str
    price: object  # Decimal, USD


class Catalog:
    def __init__(self, items):
        self._items = {}
        for item in items:
            if item.sku in self._items:
                raise CatalogError(f"duplicate SKU {item.sku}")
            self._items[item.sku] = item

    def __len__(self):
        return len(self._items)

    def __contains__(self, sku):
        return sku in self._items

    def get(self, sku):
        try:
            return self._items[sku]
        except KeyError:
            raise CatalogError(f"unknown SKU {sku}") from None

    def skus(self):
        return sorted(self._items)


def load_catalog(path):
    try:
        with open(path, newline="", encoding="utf-8") as f:
            rows = list(csv.DictReader(f))
    except FileNotFoundError:
        raise CatalogError(f"no such catalog: {path}") from None
    items = []
    for number, row in enumerate(rows, start=2):
        try:
            sku = row["sku"].strip()
            if not sku:
                raise ValueError("empty SKU")
            items.append(Item(sku=sku, name=row["name"].strip(), price=parse_price(row["price"])))
        except (KeyError, AttributeError, ValueError) as e:
            raise CatalogError(f"{path}, line {number}: {e}") from None
    return Catalog(items)
