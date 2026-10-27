import unittest

from todo import TodoList


class TodoListTest(unittest.TestCase):
    def test_add_numbers_items(self):
        todos = TodoList()
        self.assertEqual(todos.add("a")["id"], 1)
        self.assertEqual(todos.add("b")["id"], 2)

    def test_done_removes_from_pending(self):
        todos = TodoList()
        todos.add("a")
        todos.add("b")
        todos.done(1)
        self.assertEqual([i["title"] for i in todos.pending()], ["b"])

    def test_done_unknown_id(self):
        with self.assertRaises(KeyError):
            TodoList().done(9)


if __name__ == "__main__":
    unittest.main()
