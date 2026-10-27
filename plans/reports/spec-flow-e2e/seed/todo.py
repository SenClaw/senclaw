"""A tiny to-do list kept in memory."""


class TodoList:
    def __init__(self):
        self.items = []

    def add(self, title):
        item = {"id": len(self.items) + 1, "title": title, "done": False}
        self.items.append(item)
        return item

    def done(self, item_id):
        for item in self.items:
            if item["id"] == item_id:
                item["done"] = True
                return item
        raise KeyError(item_id)

    def pending(self):
        return [i for i in self.items if not i["done"]]
