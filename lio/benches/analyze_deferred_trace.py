"""Count ring entries in the marked interval of trace_deferred.py output."""
from collections import Counter, defaultdict
from pathlib import Path
import re
import sys

for filename in sys.argv[1:]:
    trace = Path(filename).read_text()
    window = trace.split("tracing_mark_write: lio_start\n", 1)[1]
    window = window.split("tracing_mark_write: lio_end", 1)[0]
    events = Counter()
    entries = Counter()
    elapsed = defaultdict(float)
    pending = {}
    for line in window.splitlines():
        if ": " in line:
            events[line.split(": ", 1)[1].split(":", 1)[0]] += 1
        match = re.search(r"-(\d+)\s+\[.*?\s(\d+\.\d+): sys_io_uring_enter(.*)", line)
        if not match:
            continue
        tid, timestamp, tail = match.groups()
        timestamp = float(timestamp)
        if tail.startswith("(fd:"):
            submit = re.search(r"to_submit: ([^,]+)", tail)[1]
            wait = re.search(r"min_complete: ([^,]+)", tail)[1]
            key = (int(submit, 0), int(wait, 0))
            entries[key] += 1
            pending[tid] = (timestamp, key)
        elif tid in pending:
            start, key = pending.pop(tid)
            elapsed[key] += (timestamp - start) * 1e6
    print(filename)
    print("events:", dict(events))
    for key, count in entries.items():
        print(f"submit={key[0]} min_complete={key[1]} count={count} "
              f"total_traced_us={elapsed[key]:.3f}")
    if pending:
        raise SystemExit("Incomplete syscall interval")
