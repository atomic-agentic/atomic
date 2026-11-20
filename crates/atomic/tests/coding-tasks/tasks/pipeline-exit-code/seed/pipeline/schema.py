"""The files the pipeline reads, and the rules an order must keep."""

import datetime
import re
from decimal import Decimal, InvalidOperation

CUSTOMER_COLUMNS = ("customer_id", "name", "region")
ORDER_COLUMNS = ("order_id", "customer_id", "date", "amount", "currency")

CUSTOMERS = "customers.csv"
ORDERS = "orders.csv"
INPUTS = ((CUSTOMERS, CUSTOMER_COLUMNS), (ORDERS, ORDER_COLUMNS))

_CURRENCY = re.compile(r"[A-Z]{3}")
_DATE = re.compile(r"\d{4}-\d{2}-\d{2}")
_AMOUNT = re.compile(r"\d+(\.\d{1,2})?")


def customer_problems(row):
    """What is wrong with a customer row, as a list of messages (empty when nothing is)."""
    problems = []
    if not row["customer_id"]:
        problems.append("customer without an id")
    if not row["name"]:
        problems.append(f"customer {row['customer_id']}: no name")
    return problems


def order_problems(row, customer_ids, seen_ids):
    """What is wrong with an order row. Adds its id to seen_ids."""
    problems = []
    order_id = row["order_id"]
    if not order_id.isdigit() or int(order_id) == 0:
        problems.append(f"order {order_id!r}: id is not a positive integer")
    elif order_id in seen_ids:
        problems.append(f"order {order_id}: duplicate id")
    seen_ids.add(order_id)
    if row["customer_id"] not in customer_ids:
        problems.append(f"order {order_id}: unknown customer {row['customer_id']}")
    if not valid_date(row["date"]):
        problems.append(f"order {order_id}: bad date {row['date']!r}")
    if not valid_amount(row["amount"]):
        problems.append(f"order {order_id}: bad amount {row['amount']!r}")
    if not _CURRENCY.fullmatch(row["currency"]):
        problems.append(f"order {order_id}: bad currency {row['currency']!r}")
    return problems


def valid_date(text):
    if not _DATE.fullmatch(text):
        return False
    try:
        datetime.date.fromisoformat(text)
    except ValueError:
        return False
    return True


def valid_amount(text):
    if not _AMOUNT.fullmatch(text):
        return False
    try:
        return Decimal(text) >= 0
    except InvalidOperation:
        return False
