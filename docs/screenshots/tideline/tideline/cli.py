"""The command line: `tideline <harbour>`."""

import sys

from tideline.client import fetch
from tideline.table import render


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: tideline <harbour>", file=sys.stderr)
        return 2
    print(render(fetch(sys.argv[1])))
    return 0
