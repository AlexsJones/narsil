"""Lay the tides out as a table."""


def render(tides: list[dict]) -> str:
    rows = [f"{t['time']}  {t['kind']:<5} {t['height']:>4.1f} m" for t in tides]
    return "\n".join(rows)
