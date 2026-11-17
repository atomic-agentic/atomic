# SPDX-License-Identifier: Apache-2.0
import json
import sys


def main():
    order = json.load(sys.stdin)
    # The runtime validates schemas; these checks also protect direct script use.
    lines = order['lines']
    if not lines or len(lines) > 100:
        raise ValueError('expected between 1 and 100 lines')
    for line in lines:
        quantity, unit = line['quantity'], line['unit_cents']
        if type(quantity) is not int or not 1 <= quantity <= 10000:
            raise ValueError('quantity must be an integer from 1 to 10000')
        if type(unit) is not int or not 0 <= unit <= 10000000:
            raise ValueError('unit_cents must be a nonnegative integer within the schema limit')
    json.dump({'order_id': order['order_id'],
               'total_cents': sum(x['quantity'] * x['unit_cents'] for x in lines),
               'item_count': sum(x['quantity'] for x in lines)}, sys.stdout)
    sys.stdout.write('\n')


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, TypeError) as exc:
        print(str(exc), file=sys.stderr)
        sys.exit(1)
