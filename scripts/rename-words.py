#!/usr/bin/env python3
"""Whole-word renames in the files given: rename-words.py OLD=NEW [OLD=NEW ...] -- FILE...

Each OLD is matched as a whole word (identifier boundaries, so `point` matches `point` and
`Point` only where given, not `pointer` or `checkpoint`), longest first, in one pass per file.
"""
import re
import sys

args = sys.argv[1:]
split = args.index("--")
pairs = [pair.split("=", 1) for pair in args[:split]]
files = args[split + 1 :]
pairs.sort(key=lambda pair: -len(pair[0]))
table = dict(pairs)
pattern = re.compile(r"(?<![A-Za-z0-9])(" + "|".join(re.escape(old) for old, _ in pairs) + r")(?![A-Za-z0-9])")
for path in files:
    with open(path) as f:
        text = f.read()
    new = pattern.sub(lambda m: table[m.group(1)], text)
    if new != text:
        with open(path, "w") as f:
            f.write(new)
        print(path)
