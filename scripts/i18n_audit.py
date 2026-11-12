"""Lists, per locale, texts left identical to English and values naming \"Files\" where the English
doesn't. Usage: python scripts/i18n_audit.py crates/rf-app/locales"""
# Per locale: entries left identical to English (likely untranslated) and values that name
# "Files" when the English doesn't (Files' own strings leaking in).
import json
import pathlib
import re
import sys

# Windows consoles default to a legacy code page; texts here are in every script
sys.stdout.reconfigure(encoding="utf-8")

loc = pathlib.Path(sys.argv[1])
src = json.loads((loc / "_source.json").read_text(encoding="utf-8"))
# texts that are fine to keep as English everywhere
keep = re.compile(r"^(smoothyfiles|DirectLook|GPUI|Git|GitHub|Mica|Mica Alt|Acrylic|ZIP|RAR|7z|TAR|OK|PDF|SVG|PowerShell|Velopack|Unicode|SHA-?\d+|MD5|CRC32|[A-Z0-9+\-./ ]{1,6}|.*Ctrl.*|.*\{[^}]*\}.*)$")
report = {}
for f in sorted(loc.glob("*.json")):
    if f.name.startswith("_") or f.stem == "en-GB":
        continue
    d = json.loads(f.read_text(encoding="utf-8"))
    same = [e for e in src if d.get(e["id"]) == e["en"] and not keep.match(e["en"])]
    files = [e for e in src if "Files" in d.get(e["id"], "") and "Files" not in e["en"]]
    report[f.stem] = (same, files)
for code, (same, files) in sorted(report.items(), key=lambda kv: -len(kv[1][0])):
    print(f"{code:8} same-as-English {len(same):3}  'Files' leaks {len(files)}", ("e.g. " + "; ".join(e["en"] for e in same[:4])) if same else "")
    for e in files:
        print(f"           FILES {e['id']} {e['en']!r} -> {json.loads((loc / (code + '.json')).read_text(encoding='utf-8'))[e['id']]!r}")
