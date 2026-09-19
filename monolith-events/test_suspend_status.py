#!/usr/bin/env python3
import unittest
from monolith_suspend_status import property_value


class PropertyValueTests(unittest.TestCase):
    def test_reads_named_property(self):
        self.assertEqual(property_value("IdleHint=yes\nState=active\n", "IdleHint"), "yes")

    def test_returns_empty_for_missing_property(self):
        self.assertEqual(property_value("State=active\n", "IdleHint"), "")


if __name__ == "__main__":
    unittest.main()
