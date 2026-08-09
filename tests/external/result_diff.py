#!/usr/bin/env python3
"""Comparison primitives shared by PostgreSQL differential drivers."""

import datetime
import unittest


def has_outer_order_by(sql):
    """Whether a statement has an ORDER BY at parenthesis depth zero.

    The drivers may disregard row order only when SQL leaves it unspecified.
    This scanner recognizes SQL quoting and comments so text or nested window
    clauses cannot accidentally make an unordered result look ordered.
    """
    words = []
    word = []
    depth = 0
    index = 0
    length = len(sql)

    def finish_word():
        if word:
            words.append((depth, "".join(word).lower()))
            word.clear()

    while index < length:
        char = sql[index]
        next_char = sql[index + 1] if index + 1 < length else ""
        if char == "'":
            finish_word()
            index += 1
            while index < length:
                if sql[index] == "'":
                    if index + 1 < length and sql[index + 1] == "'":
                        index += 2
                        continue
                    index += 1
                    break
                index += 1
            continue
        if char == '"':
            finish_word()
            index += 1
            while index < length:
                if sql[index] == '"':
                    if index + 1 < length and sql[index + 1] == '"':
                        index += 2
                        continue
                    index += 1
                    break
                index += 1
            continue
        if char == "-" and next_char == "-":
            finish_word()
            index = sql.find("\n", index + 2)
            if index < 0:
                break
            continue
        if char == "/" and next_char == "*":
            finish_word()
            index = sql.find("*/", index + 2)
            if index < 0:
                return False
            index += 2
            continue
        if char == "(":
            finish_word()
            depth += 1
        elif char == ")":
            finish_word()
            depth = max(0, depth - 1)
        elif char.isalnum() or char == "_":
            word.append(char)
        else:
            finish_word()
        index += 1
    finish_word()
    return any(
        first_depth == 0 and first == "order" and second_depth == 0 and second == "by"
        for (first_depth, first), (second_depth, second) in zip(words, words[1:])
    )


def cell_key(value):
    """Produce a collision-free comparison key for a decoded wire value."""
    if value is None:
        return ("null",)
    if isinstance(value, (bytes, bytearray, memoryview)):
        return ("bytes", bytes(value))
    if isinstance(value, (datetime.datetime, datetime.date, datetime.time)):
        # psycopg may materialize the identical UTC wire value with either
        # datetime.timezone.utc or ZoneInfo("Etc/UTC"). The temporal value,
        # not the client-side timezone object's implementation, is observable.
        return (type(value).__module__, type(value).__qualname__, value.isoformat())
    if isinstance(value, tuple):
        return ("tuple", tuple(cell_key(item) for item in value))
    if isinstance(value, list):
        return ("list", tuple(cell_key(item) for item in value))
    return (type(value).__module__, type(value).__qualname__, repr(value))


def rows_key(rows, ordered):
    """Compare rows exactly when order is specified, otherwise as a multiset."""
    if rows is None:
        return None
    result = [tuple(cell_key(value) for value in row) for row in rows]
    if not ordered:
        result.sort(key=repr)
    return result


class ResultDiffTests(unittest.TestCase):
    def test_outer_order_by_ignores_quoted_and_nested_words(self):
        self.assertTrue(has_outer_order_by("SELECT 'order by' FROM data ORDER BY id"))
        self.assertTrue(has_outer_order_by("SELECT * FROM data -- order by\nORDER BY id"))
        self.assertFalse(has_outer_order_by("SELECT row_number() OVER (ORDER BY id) FROM data"))
        self.assertFalse(has_outer_order_by("SELECT * FROM (SELECT * FROM data ORDER BY id) AS nested"))

    def test_ordered_rows_preserve_order(self):
        first = [(1,), (2,)]
        second = [(2,), (1,)]
        self.assertNotEqual(rows_key(first, ordered=True), rows_key(second, ordered=True))
        self.assertEqual(rows_key(first, ordered=False), rows_key(second, ordered=False))

    def test_cell_keys_preserve_null_type_and_bytes_identity(self):
        self.assertNotEqual(cell_key(None), cell_key("None"))
        self.assertNotEqual(cell_key(1), cell_key("1"))
        self.assertNotEqual(cell_key(b"x"), cell_key("x"))

    def test_temporal_keys_ignore_equivalent_timezone_implementation(self):
        plain_utc = datetime.datetime(2020, 1, 1, tzinfo=datetime.timezone.utc)
        class EquivalentUtc(datetime.tzinfo):
            def utcoffset(self, value):
                return datetime.timedelta(0)
            def dst(self, value):
                return datetime.timedelta(0)
        alternate_utc = datetime.datetime(2020, 1, 1, tzinfo=EquivalentUtc())
        self.assertEqual(cell_key(plain_utc), cell_key(alternate_utc))


if __name__ == "__main__":
    unittest.main()
