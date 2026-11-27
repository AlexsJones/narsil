#!/usr/bin/env python3
"""Write the scripts the demonstration follows: what the stand-in model does for each
request typed in make.sh. See the top of src/provider/mock.rs for the format."""
import json, pathlib, sys

def tool(name, **args):
    return f"!tool {name} {json.dumps(args)}"

BACKOFF = '''"""Wait longer after each failure."""

import time


def retrying(call, attempts: int = 3, first_wait: float = 0.5, sleep=time.sleep):
    """Run `call`; after a failure, again, waiting twice as long each time."""
    wait = first_wait
    for attempt in range(1, attempts + 1):
        try:
            return call()
        except OSError:
            if attempt == attempts:
                raise
            sleep(wait)
            wait *= 2
'''

TESTS = '''import unittest

from tideline.backoff import retrying


class RetryingTest(unittest.TestCase):
    def test_a_call_that_works_is_made_once(self):
        calls = []
        self.assertEqual(retrying(lambda: calls.append(1) or "tides"), "tides")
        self.assertEqual(len(calls), 1)

    def test_it_tries_again_and_waits_twice_as_long(self):
        waits, left = [], [OSError("busy"), OSError("busy")]

        def flaky():
            if left:
                raise left.pop()
            return "tides"

        self.assertEqual(retrying(flaky, sleep=waits.append), "tides")
        self.assertEqual(waits, [0.5, 1.0])

    def test_it_gives_up_after_the_last_attempt(self):
        def down():
            raise OSError("down")

        with self.assertRaises(OSError):
            retrying(down, sleep=lambda _: None)


if __name__ == "__main__":
    unittest.main()
'''

OLD_BODY = '''    with urllib.request.urlopen(FEED.format(harbour=harbour), timeout=timeout) as reply:
        return json.load(reply)["tides"]
'''
NEW_BODY = '''    def once() -> list[dict]:
        with urllib.request.urlopen(FEED.format(harbour=harbour), timeout=timeout) as reply:
            return json.load(reply)["tides"]

    # The feed drops connections when it is busy: try again, waiting longer each time.
    return retrying(once)
'''
README_OLD = "The times come from the harbour board's public feed.\n"
README_NEW = README_OLD + "\nThe feed drops connections when it is busy, so each request is tried three times,\nwaiting half a second and then a second, before tideline gives up.\n"

def nested(steps):
    return " ~~ ".join(steps)

tests_agent = nested([
    tool("work_next", claim="Cover the retry with tests"),
    tool("read_file", path="tideline/backoff.py"),
    tool("write_file", path="tests/test_backoff.py", content=TESTS),
    "!after 30 !bash python3 -m unittest -q 2>&1 | tail -3",
    tool("work_done", item="Cover the retry with tests", result="tests/test_backoff.py: one call when it works, the doubling wait, giving up.", evidence="python3 -m unittest: 4 tests, OK", learned=[]),
    "!say The retry is covered by three tests, and all four in the project pass.",
])
docs_agent = nested([
    tool("work_next", claim="Say how it behaves in the README"),
    tool("read_file", path="README.md"),
    "!after 30 " + tool("edit_file", path="README.md", old_string=README_OLD, new_string=README_NEW),
    tool("work_done", item="Say how it behaves in the README", result="README.md says the feed is tried three times.", evidence="read back", learned=[]),
    "!say The README says how the retry behaves.",
])

retry = [
    tool("work_plan", directory=".", root="Retry with backoff", items=[
        {"title": "Add backoff to the client"},
        {"title": "Cover the retry with tests", "blocked_by": ["Add backoff to the client"]},
        {"title": "Say how it behaves in the README", "blocked_by": ["Add backoff to the client"]},
    ]),
    tool("read_file", path="tideline/client.py"),
    tool("read_file", path="tideline/cli.py"),
    tool("work_next", claim="Add backoff to the client"),
    tool("write_file", path="tideline/backoff.py", content=BACKOFF),
    tool("edit_file", path="tideline/client.py", old_string="import urllib.request\n", new_string="import urllib.request\n\nfrom tideline.backoff import retrying\n"),
    tool("edit_file", path="tideline/client.py", old_string=OLD_BODY, new_string=NEW_BODY),
    "!bash python3 -c 'from tideline.client import fetch; print(fetch.__doc__)'",
    tool("work_done", item="Add backoff to the client", result="fetch tries three times, waiting 0.5s then 1s.", evidence="imported and read back", learned=[]),
    tool("spawn_agents", agents=[
        {"name": "tests", "work": "Cover the retry with tests", "task": tests_agent},
        {"name": "docs", "work": "Say how it behaves in the README", "task": docs_agent},
    ]),
    "!bash python3 -m unittest -q 2>&1 | tail -3",
    "!say `fetch` now tries three times, waiting half a second and then a second, before it gives up with the feed's own error.\n\n"
    "- `tideline/backoff.py` holds the retry, apart from the client, so it is tested without a network.\n"
    "- `tests/test_backoff.py` covers it: one call when it works, the doubling wait, and giving up.\n"
    "- The README says how it behaves.\n\n"
    "All four tests pass.",
]

down = [
    tool("read_file", path="tideline/client.py"),
    tool("read_file", path="tideline/backoff.py"),
    "!say It waits a second and a half in all, then fails with the error the feed gave: `retrying` re-raises the last `OSError`, and `cli.main` does not catch it, so the user sees a traceback.\n\n"
    "That is the one rough edge left. Catching it in `main` and printing `tideline: the feed is not answering` would be a three-line change. Say if you want it.",
]

again = [
    "!bash python3 -m unittest -q 2>&1 | tail -3",
    "!say All four pass.",
]

blocks = [("retry with backoff", retry), ("feed is down", down), ("run the tests", again)]
out = "\n====\n".join(f"? {key}\n" + "\n;;\n".join(steps) for key, steps in blocks)
pathlib.Path(sys.argv[1]).write_text(out + "\n")
print("steps:", [len(s) for _, s in blocks])
