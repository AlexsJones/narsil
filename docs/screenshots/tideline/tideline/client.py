"""Fetch tide times from the harbour board's feed."""

import json
import urllib.request

FEED = "https://tides.example.org/v1/harbours/{harbour}.json"


def fetch(harbour: str, timeout: float = 5.0) -> list[dict]:
    """The day's tides for a harbour: a list of {time, height, kind}."""
    with urllib.request.urlopen(FEED.format(harbour=harbour), timeout=timeout) as reply:
        return json.load(reply)["tides"]
