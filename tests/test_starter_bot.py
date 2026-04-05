"""
tests/test_starter_bot.py
Tests for starter_bot/strategy.py

T_BOT_01: on_batch_start with balance → returns 1-2 orders near 0.05
T_BOT_02: on_reveal with 3 commits → returns all 3 (reveal all)
"""

import sys
import os
import unittest
from decimal import Decimal

# Add parent to path so we can import starter_bot
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from starter_bot.strategy import on_batch_start, on_reveal


class TestStarterBot(unittest.TestCase):

    # T_BOT_01: on_batch_start with ctx → returns 1-2 orders near 0.05
    def test_bot_01_batch_start_generates_orders(self):
        ctx = {
            "batch_id": 100,
            "pool_id": 2,
            "escrow": {
                "KAY": "500.00000000",
                "EMM": "10000.00000000",
            },
            "last_batch": {
                "batch_id": 99,
                "matches": 3,
                "volume": "1500.00000000",
            },
        }

        orders = on_batch_start(ctx)

        # Should return 1-2 orders (buy and/or sell)
        self.assertGreaterEqual(len(orders), 1)
        self.assertLessEqual(len(orders), 2)

        # Check order structure
        for order in orders:
            self.assertEqual(order["pair"], "EMM/KAY")
            self.assertIn(order["side"], ["buy", "sell"])
            price = Decimal(order["price"])
            qty = Decimal(order["quantity"])
            self.assertGreater(price, 0)
            self.assertGreater(qty, 0)

        # Prices should be near 0.05 (within spread)
        for order in orders:
            price = Decimal(order["price"])
            self.assertGreater(price, Decimal("0.04"))
            self.assertLess(price, Decimal("0.06"))

        # Should have both buy and sell when both balances exist
        sides = {o["side"] for o in orders}
        self.assertEqual(sides, {"buy", "sell"})

    # T_BOT_02: on_reveal with 3 commits → returns all 3
    def test_bot_02_reveal_all(self):
        ctx = {}
        my_commits = ["commit_hash_0", "commit_hash_1", "commit_hash_2"]
        indices = on_reveal(ctx, my_commits)

        self.assertEqual(indices, [0, 1, 2])

    # Extra: no balance → no orders
    def test_no_balance_no_orders(self):
        ctx = {
            "escrow": {
                "KAY": "0.00000000",
                "EMM": "0.00000000",
            },
        }
        orders = on_batch_start(ctx)
        self.assertEqual(len(orders), 0)

    # Extra: only quote balance → only buy order
    def test_only_quote_balance(self):
        ctx = {
            "escrow": {
                "KAY": "100.00000000",
                "EMM": "0.00000000",
            },
        }
        orders = on_batch_start(ctx)
        self.assertEqual(len(orders), 1)
        self.assertEqual(orders[0]["side"], "buy")


if __name__ == "__main__":
    unittest.main()
