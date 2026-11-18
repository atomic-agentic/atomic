# SPDX-License-Identifier: Apache-2.0
import json
import sys


def main():
    data = json.load(sys.stdin)
    total, assessment = data['total'], data['assessment']
    if (total['order_id'], total['total_cents']) != (assessment['order_id'], assessment['total_cents']):
        raise ValueError('assessment must refer to the computed order and total')
    report = (f"Order {total['order_id']}: {total['item_count']} items, "
              f"{total['total_cents']} cents. {assessment['note']}")
    json.dump({'order_id': total['order_id'], 'report': report}, sys.stdout, ensure_ascii=False)
    sys.stdout.write('\n')


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, TypeError) as exc:
        print(str(exc), file=sys.stderr)
        sys.exit(1)
