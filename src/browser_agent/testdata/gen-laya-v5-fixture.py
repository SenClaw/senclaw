"""Regenerate laya_v5.json: format-v5 decision requests as laya-browser v19s's own code builds them.

    python3 gen-laya-v5-fixture.py <path to laya_browser.py> > laya_v5.json

`laya_browser.py` is the model card's file at the commit the catalog pins
(huggingface.co/cklxx/laya-browser, 645cf366a2ae35f1086e8c20eff48f909bb49206); it needs
nothing beyond the standard library to build a request.

Each case holds a page as sen-browser observes it and the request `build_request` makes of
the same page as the model's harness observed it: no "go back" control, and Enter offered as
`press_enter` under the harness's label, only when the focused field holds text
(`enter_offered`, decided by hand per case). `encoder.rs` must turn the first into the second.
The wide case also records `predict_chunked` asking a deterministic stand-in for the model,
which `decide.rs` replays.
"""
import importlib.util
import json
import sys

PRESS_ENTER_LABEL = "Press Enter in the focused text field (submit it)"


def load(path):
    spec = importlib.util.spec_from_file_location("laya_browser", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def field(i, node, label, value, role="textbox", kind="fill"):
    return {"id": f"e{i}", "node": node, "kind": kind, "role": role, "label": label, "value": value}


def scroll_down():
    return {"id": "scroll_down", "kind": "scroll", "label": "Scroll down", "delta": 560}


def scroll_up():
    return {"id": "scroll_up", "kind": "scroll", "label": "Scroll up", "delta": -560}


def enter(name):
    return {"id": "key_enter", "kind": "key", "key": "Enter", "label": "Press Enter in " + name}


GO_BACK = {"id": "go_back", "kind": "back", "label": "Go back to the previous page"}
WAIT = {"id": "wait", "kind": "wait", "label": "Wait for the page to update"}


def cases():
    search = [
        field(1, 1, "Search packages", "json", role="searchbox"),
        {**field(2, 1, "Open Search packages", "json", role="searchbox", kind="click")},
        {"id": "e3", "node": 2, "kind": "click", "role": "button", "label": "Search", "value": ""},
        {"id": "e4", "node": 3, "kind": "click", "role": "checkbox", "label": "Include non-root modules",
         "checked": "false", "value": "on"},
        {"id": "e5", "node": 4, "kind": "click", "role": "link", "label": "argo", "value": ""},
        {"id": "e6", "node": 5, "kind": "click", "role": "link",
         "label": "rapidjson — a very fast JSON parser and generator for Lua, by xpol", "value": ""},
        {"id": "e7", "node": 6, "kind": "click", "role": "link", "label": "Don't miss: the 'fast' parser", "value": ""},
        scroll_down(), enter("Search packages"), GO_BACK, WAIT,
    ]
    form = [
        field(1, 1, "Where from?", "Zurich", role="combobox"),
        field(2, 1, "Open Where from?", "Zurich", role="combobox", kind="click"),
        field(3, 2, "Where to?", "", role="combobox"),
        field(4, 2, "Open Where to?", "", role="combobox", kind="click"),
        {"id": "e5", "node": 3, "kind": "select", "role": "combobox", "label": "Cabin class → Economy",
         "value": "economy", "current_value": "Premium economy"},
        {"id": "e6", "node": 3, "kind": "select", "role": "combobox", "label": "Cabin class → Business",
         "value": "business", "current_value": "Premium economy"},
        {"id": "e7", "node": 3, "kind": "select", "role": "combobox",
         "label": "Cabin class   preferred  → First class   with lie-flat seats and lounge access included",
         "value": "first", "current_value": "Premium economy"},
        {"id": "e8", "node": 4, "kind": "click", "role": "radio", "label": "One-way", "checked": "false", "value": "on"},
        {"id": "e9", "node": 5, "kind": "click", "role": "radio", "label": "Round trip", "checked": "true", "value": "on"},
        field(10, 6, "Promo code", "  SAVE 10 %  "),
        field(11, 7, "Additional notes for the travel agent about seating preferences",
              "He said \"hi\" and it's fine\nreally"),
        {"id": "e12", "node": 8, "kind": "click", "role": "button", "label": "Search flights", "value": ""},
        enter("Where to?"), WAIT,
    ]
    feed = [
        {"id": "e1", "node": 1, "kind": "click", "role": "link", "label": "@mai", "value": ""},
        {"id": "e2", "node": 2, "kind": "click", "role": "button", "label": "Thích video", "checked": "false", "value": ""},
        {"id": "e3", "node": 3, "kind": "click", "role": "button", "label": "Bình luận", "value": ""},
        field(4, 4, "Tìm kiếm", "", role="searchbox"),
        field(5, 4, "Open Tìm kiếm", "", role="searchbox", kind="click"),
        scroll_down(), scroll_up(), enter("Tìm kiếm"), GO_BACK, WAIT,
    ]
    many = [field(i, i, f"Field number {i}", f"value {i}" if i % 3 else "") for i in range(1, 17)]
    many.append({"id": "e17", "node": 17, "kind": "click", "role": "switch", "label": "Dark mode", "checked": "true", "value": ""})
    many.append(WAIT)
    wide = [field(1, 1, "Search", "laptops", role="searchbox")]
    wide += [{"id": f"e{i}", "node": i, "kind": "click", "role": "link", "label": f"Result {i - 1}: laptop model {i * 37 % 1000}",
              "value": ""} for i in range(2, 132)]
    wide += [scroll_down(), enter("Search"), WAIT]
    return [
        {"name": "search", "goal": "Search packages for 'json' and open the package 'argo'.",
         "url": "https://luarocks.org/search?q=json", "title": "Search results - LuaRocks",
         "text": "Search results for json · argo 1.2 · rapidjson 1.1 · Include non-root modules",
         "history": [{"action": "Search packages", "kind": "fill", "text": "json", "page_changed": False},
                     {"action": "Press Enter in Search packages", "kind": "key", "text": None, "page_changed": True}],
         "actions": search, "enter_offered": True},
        {"name": "form", "goal": "Find a one-way business class flight from Zurich to Hanoi.",
         "url": "https://flights.example.com/", "title": "Find flights",
         "text": "Where from? Where to? Departure Return Passengers Cabin class Search flights", "history": [],
         "actions": form, "enter_offered": False},
        {"name": "feed", "goal": "Mở video của @mai rồi bấm thích.", "url": "https://www.tiktok.com/explore",
         "title": "Khám phá | TikTok", "text": "Dành cho bạn · @mai Món ngon mỗi ngày 🍜 #nauan · 12,3K lượt thích",
         "history": [{"action": "Scroll down", "kind": "scroll", "text": None, "page_changed": True}],
         "actions": feed, "enter_offered": False},
        {"name": "many_fields", "goal": "Fill in the form.", "url": "https://forms.example.com/a", "title": "Form",
         "text": "A long form " * 150, "history": [], "actions": many, "enter_offered": False},
        {"name": "wide", "goal": "Open the cheapest laptop.", "url": "https://shop.example.com/s?q=laptops",
         "title": "laptops - Shop", "text": "Results for laptops", "history": [], "actions": wide, "enter_offered": True},
    ]


def as_trained(case):
    """The same page as the model's harness observed it."""
    actions = []
    for a in case["actions"]:
        if a["id"] == "go_back":
            continue
        if a["id"] == "key_enter":
            if case["enter_offered"]:
                actions.append({"id": "press_enter", "kind": "key", "label": PRESS_ENTER_LABEL, "key": "Enter"})
            continue
        actions.append(a)
    page = {"url": case["url"], "title": case["title"], "text": case["text"], "actions": actions}
    history = [{**h, "action": PRESS_ENTER_LABEL} if h["kind"] == "key" else h for h in case["history"]]
    return page, history


class StandIn:
    """A deterministic model: option i of question `qid` weighs ((7i + len(qid)) mod 11) + 1."""

    def __init__(self):
        self.passes = []

    def predict(self, state, questions):
        self.passes.append({qid: list(q["criteria"]) for qid, q in questions.items()})
        answers = {}
        for qid, q in questions.items():
            keys = list(q["criteria"])
            weights = [((7 * i + len(qid)) % 11) + 1 for i in range(len(keys))]
            total = sum(weights)
            probabilities = {k: w / total for k, w in zip(keys, weights)}
            choice = max(probabilities, key=probabilities.get)
            answers[qid] = {"type": "choice", "choice": choice, "probabilities": probabilities,
                            "confidence": probabilities[choice]}
        return {"answers": answers}


def main():
    lb = load(sys.argv[1])
    out = []
    for case in cases():
        page, history = as_trained(case)
        state, questions, _, _ = lb.build_request(page, case["goal"], history)
        entry = {
            "name": case["name"],
            "goal": case["goal"],
            "history": case["history"],
            "page": {"url": case["url"], "title": case["title"], "text": case["text"], "actions": case["actions"]},
            "state": state,
            "questions": questions,
        }
        if case["name"] == "wide":
            model = StandIn()
            answers = lb.predict_chunked(model, state, questions, maxopt=lb.MAXOPT)
            entry["chunked"] = {"maxopt": lb.MAXOPT, "passes": model.passes,
                                "answers": {qid: {k: a[k] for k in ("choice", "probabilities")} for qid, a in answers.items()}}
        out.append(entry)
    json.dump({"source": "cklxx/laya-browser@645cf366a2ae35f1086e8c20eff48f909bb49206 laya_browser.py",
               "cases": out}, sys.stdout, ensure_ascii=False, indent=1)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
