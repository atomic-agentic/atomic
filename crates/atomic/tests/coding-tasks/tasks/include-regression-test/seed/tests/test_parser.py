import unittest

from deployctl.parser import Item, ParseError, parse_text, unquote


class Parser(unittest.TestCase):
    def test_items(self):
        text = "# top\ninclude = a.conf\n\n[service]\nname = x\n; note\nimage = \"r/x:1\"\n"
        self.assertEqual(parse_text(text, "t.conf"), [
            Item("include", 2, value="a.conf"),
            Item("section", 4, "service"),
            Item("set", 5, "name", "x"),
            Item("set", 7, "image", "r/x:1"),
        ])

    def test_unquote(self):
        self.assertEqual(unquote('"a \\"b\\""'), 'a "b"')
        self.assertEqual(unquote('"'), '"')
        self.assertEqual(unquote("plain"), "plain")

    def test_errors_name_the_line(self):
        for text, line in (("[Bad Name]\n", 1), ("[service]\nnot a setting\n", 2), ("name = x\n", 1),
                           ("[a]\ninclude =\n", 2)):
            with self.assertRaises(ParseError) as caught:
                parse_text(text, "f.conf")
            self.assertEqual(caught.exception.line, line, text)
            self.assertTrue(str(caught.exception).startswith(f"f.conf:{line}: "))


if __name__ == "__main__":
    unittest.main()
