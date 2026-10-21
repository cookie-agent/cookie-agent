"""Writes the web page's emoji picker list (crates/web/src/emoji/data.rs)
from Unicode's emoji-test.txt, which names every emoji and puts it in a
group and a subgroup.

The picker shows a few hundred common emoji, so the list keeps:
fully-qualified emoji only, none with a skin tone or a zero-width joiner,
none newer than Emoji 13.0 (so that system fonts draw them), and from each
group a share of each subgroup up to the group's cap, with no
country flags. Each entry is the
emoji and its lowercase Unicode name, which the search box matches. Run it
after changing the caps:

    curl -o /tmp/emoji-test.txt https://unicode.org/Public/emoji/latest/emoji-test.txt
    python3 scripts/web-emoji.py /tmp/emoji-test.txt
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "crates", "web", "src", "emoji", "data.rs")
NEWEST = 13.0
# The picker's groups in emoji-test.txt order, each with how many emoji it
# keeps, and the label the picker shows.
CAPS = {
    "Smileys & Emotion": (90, "Smileys"),
    "People & Body": (40, "People"),
    "Animals & Nature": (70, "Animals & nature"),
    "Food & Drink": (70, "Food & drink"),
    "Travel & Places": (60, "Travel & places"),
    "Activities": (50, "Activities"),
    "Objects": (80, "Objects"),
    "Symbols": (40, "Symbols"),
    "Flags": (6, "Flags"),
}
# Groups that keep only some subgroups: no country flags.
ONLY = {"Flags": {"flag"}}


def read(path):
    """The kept emoji of `path` as {group: {subgroup: [(emoji, name)]}}."""
    groups = {}
    group = subgroup = None
    with open(path, encoding="utf-8") as lines:
        for line in lines:
            if line.startswith("# group:"):
                group = line[len("# group:"):].strip()
                continue
            if line.startswith("# subgroup:"):
                subgroup = line[len("# subgroup:"):].strip()
                continue
            if not line.strip() or line.startswith("#"):
                continue
            points, rest = line.split(";", 1)
            status, comment = rest.split("#", 1)
            if status.strip() != "fully-qualified" or group not in CAPS:
                continue
            if group in ONLY and subgroup not in ONLY[group]:
                continue
            points = [int(point, 16) for point in points.split()]
            if 0x200D in points or any(0x1F3FB <= point <= 0x1F3FF for point in points):
                continue
            match = re.match(r"\s*\S+\s+E(\d+\.\d+)\s+(.*)", comment)
            if float(match.group(1)) > NEWEST:
                continue
            emoji = "".join(chr(point) for point in points)
            name = match.group(2).strip().lower()
            groups.setdefault(group, {}).setdefault(subgroup, []).append((emoji, name))
    return groups


def share(subgroups, cap):
    """Up to `cap` emoji of `subgroups`, each subgroup's share in proportion
    to its size and at least one, in file order."""
    total = sum(len(entries) for entries in subgroups.values())
    kept = []
    for entries in subgroups.values():
        count = max(1, round(cap * len(entries) / total))
        kept.extend(entries[:count])
    return kept[:cap]


def rust_string(text):
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"') + '"'


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: web-emoji.py <emoji-test.txt>")
    groups = read(sys.argv[1])
    out = [
        "//! The emoji picker's list, written by `scripts/web-emoji.py` from",
        "//! Unicode's `emoji-test.txt`; do not edit by hand.",
        "",
        "use super::EmojiGroup;",
        "",
        "pub static GROUPS: &[EmojiGroup] = &[",
    ]
    count = 0
    for group, (cap, label) in CAPS.items():
        kept = share(groups.get(group, {}), cap)
        count += len(kept)
        out.append("    EmojiGroup {")
        out.append(f"        name: {rust_string(label)},")
        out.append("        emoji: &[")
        for emoji, name in kept:
            out.append(f"            ({rust_string(emoji)}, {rust_string(name)}),")
        out.append("        ],")
        out.append("    },")
    out.append("];")
    with open(OUT, "w", encoding="utf-8") as file:
        file.write("\n".join(out) + "\n")
    print(f"wrote {count} emoji to {os.path.relpath(OUT, ROOT)}")


if __name__ == "__main__":
    main()
