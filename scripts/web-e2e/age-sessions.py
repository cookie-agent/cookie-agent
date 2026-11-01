"""Makes root sessions of a stopped daemon's store look as if they were last
active days ago, for the session list's window
(docs/site/specs/session-storage-v3.md sections 4.1 and 4.3). Run by
scripts/test-web-page.sh before the daemon starts:

    python3 -I age-sessions.py <data-root> <out.json> <label>:<session-id>=<days>[,unread] ...

Each session gets an id of the time it is moved back to, since a root's
folder is filed under the month of its id: the folder moves to
`sessions/<yyyy-mm>/<new-id>`, the old id is replaced in every file in it,
and every timestamp in its metadata and log goes back by `days`. Its
activity journal lines leave the journals they were in for the journal of
its new last activity's month. It stays unread when `,unread` follows,
and is read otherwise. `out.json` maps each label to the session's new id.
"""

import datetime
import json
import os
import re
import sys

TIMESTAMP = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z")


def parse(text):
    """An RFC 3339 UTC timestamp, its fraction cut to microseconds."""
    whole, _, fraction = text[:-1].partition(".")
    moment = datetime.datetime.strptime(whole, "%Y-%m-%dT%H:%M:%S").replace(tzinfo=datetime.timezone.utc)
    return moment + datetime.timedelta(microseconds=int((fraction + "000000")[:6]))


def format_time(moment):
    return moment.strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def month(moment):
    return moment.strftime("%Y-%m")


def id_time(session_id):
    """The time a UUIDv7 holds in its first 48 bits, in milliseconds."""
    millis = int(session_id.replace("-", "")[:12], 16)
    return datetime.datetime.fromtimestamp(millis / 1000, datetime.timezone.utc)


def moved_id(session_id, back):
    """`session_id` with its time `back` earlier, its random bits kept."""
    millis = int(session_id.replace("-", "")[:12], 16) - int(back.total_seconds() * 1000)
    digits = "%012x" % millis + session_id.replace("-", "")[12:]
    return "%s-%s-%s-%s-%s" % (digits[:8], digits[8:12], digits[12:16], digits[16:20], digits[20:])


def shifted(text, old, new, back):
    """`text` with `old` replaced by `new` and every timestamp `back` earlier."""
    text = text.replace(old, new)
    return TIMESTAMP.sub(lambda match: format_time(parse(match.group()) - back), text)


def rewrite_tree(folder, old, new, back):
    for directory, _, files in os.walk(folder):
        for name in files:
            if name.endswith(".lock"):
                continue
            path = os.path.join(directory, name)
            with open(path, encoding="utf-8") as file:
                text = file.read()
            with open(path, "w", encoding="utf-8") as file:
                file.write(shifted(text, old, new, back))


def main():
    data_root, out = sys.argv[1:3]
    sessions = os.path.join(data_root, "sessions")
    journals = os.path.join(sessions, "activity")
    aged = {}
    labels = {}
    unread = set()
    for argument in sys.argv[3:]:
        label, _, argument = argument.partition(":")
        session_id, _, spec = argument.partition("=")
        days, _, flag = spec.partition(",")
        back = datetime.timedelta(days=int(days))
        new = moved_id(session_id, back)
        aged[session_id] = (new, back)
        labels[label] = new
        if flag == "unread":
            unread.add(new)
        source = os.path.join(sessions, month(id_time(session_id)), session_id)
        target_month = os.path.join(sessions, month(id_time(new)))
        os.makedirs(target_month, exist_ok=True)
        target = os.path.join(target_month, new)
        os.rename(source, target)
        rewrite_tree(target, session_id, new, back)
        if not os.listdir(os.path.dirname(source)):
            os.rmdir(os.path.dirname(source))
    # Each aged session's journal lines move to its new month's journal.
    moved = {}
    for name in sorted(os.listdir(journals)):
        path = os.path.join(journals, name)
        with open(path, encoding="utf-8") as file:
            lines = file.read().splitlines()
        kept = []
        for line in lines:
            session_id = json.loads(line).get("session_id") if line.strip() else None
            if session_id in aged:
                new, back = aged[session_id]
                line = shifted(line, session_id, new, back)
                moved.setdefault(month(parse(json.loads(line)["last_activity"])), []).append(line)
            else:
                kept.append(line)
        if kept or lines == kept:
            with open(path, "w", encoding="utf-8") as file:
                file.write("".join(line + "\n" for line in kept))
        else:
            # A month left with no entry has no journal: the list would
            # offer older sessions there.
            os.remove(path)
    for name, lines in moved.items():
        with open(os.path.join(journals, name + ".jsonl"), "a", encoding="utf-8") as file:
            file.write("".join(line + "\n" for line in lines))
    path = os.path.join(data_root, "unread.json")
    with open(path, encoding="utf-8") as file:
        state = json.load(file)
    entries = []
    for entry in state["sessions"]:
        if entry["session_id"] in aged:
            new, back = aged[entry["session_id"]]
            if new not in unread:
                continue
            entry = {"session_id": new, "since": format_time(parse(entry["since"]) - back)}
        entries.append(entry)
    with open(path, "w", encoding="utf-8") as file:
        json.dump({"sessions": entries}, file, indent=2)
    with open(out, "w", encoding="utf-8") as file:
        json.dump(labels, file)


main()
