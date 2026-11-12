"""Checks locale files against crates/rf-app/locales/_source.json.

    python scripts/i18n_check.py            # every locale file
    python scripts/i18n_check.py de-DE ja-JP

A locale file is crates/rf-app/locales/<code>.json: {"<id>": "<translation>", ...} with one
entry per id in _source.json. Reports missing / unknown ids, empty values, and placeholders
({n}, {n:03}, {v}, ...) that differ from the English text. Exit code 1 if anything is wrong.
"""
import json
import pathlib
import re
import sys

# Windows consoles default to a legacy code page; texts here are in every script
sys.stdout.reconfigure(encoding="utf-8")

LOC = pathlib.Path(__file__).resolve().parent.parent / "crates" / "rf-app" / "locales"
PH = re.compile(r"\{[^{}]*\}")


def check(path, source):
    data = json.loads(path.read_text(encoding="utf-8"))
    problems = []
    ids = {e["id"] for e in source}
    for e in source:
        v = data.get(e["id"])
        if v is None:
            problems.append(f"missing {e['id']} ({e['en']!r})")
        elif not isinstance(v, str) or not v.strip():
            problems.append(f"empty {e['id']} ({e['en']!r})")
        elif sorted(PH.findall(v)) != sorted(PH.findall(e["en"])):
            problems.append(f"placeholders {e['id']}: {e['en']!r} -> {v!r}")
    problems += [f"unknown id {k}" for k in data if k not in ids]
    return problems


def main():
    source = json.loads((LOC / "_source.json").read_text(encoding="utf-8"))
    files = [LOC / f"{c}.json" for c in sys.argv[1:]] or sorted(p for p in LOC.glob("*.json") if not p.name.startswith("_"))
    bad = 0
    for f in files:
        if not f.exists():
            print(f"{f.stem}: no file")
            bad += 1
            continue
        p = check(f, source)
        print(f"{f.stem}: {'ok' if not p else str(len(p)) + ' problem(s)'}")
        for line in p[:20]:
            print("   ", line)
        bad += bool(p)
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
