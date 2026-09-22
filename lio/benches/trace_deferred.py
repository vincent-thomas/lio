"""Run an isolated Linux ftrace instance around the diagnostic read workload.

Run as root inside the Linux VM. Existing/global tracing is not modified.
Usage: trace_deferred.py OUTPUT BINARY [--deferred] [DEPTH]
"""
import os
from pathlib import Path
import subprocess
import sys

output = Path(sys.argv[1])
binary = sys.argv[2]
deferred = "--deferred" in sys.argv[3:]
depth = next((arg for arg in sys.argv[3:] if arg.isdigit()), "32")
if output.exists():
    raise SystemExit(f"Refusing to overwrite {output}")
instance = Path("/sys/kernel/tracing/instances") / f"lio-diagnostic-{os.getpid()}"
instance.mkdir()
try:
    # These are kernel control files in our private tracing instance, not
    # changes to the application or to another tracing session.
    (instance / "tracing_on").write_text("0")
    (instance / "buffer_size_kb").write_text("4096")
    events = ["syscalls/sys_enter_io_uring_enter/enable",
              "syscalls/sys_exit_io_uring_enter/enable"]
    if not os.environ.get("LIO_TRACE_SYSCALLS_ONLY"):
        events += ["io_uring/enable", "sched/sched_switch/enable"]
    for event in events:
        (instance / "events" / event).write_text("1")
    env = os.environ.copy()
    env["LIO_DIAG_DEPTH"] = depth
    env["LIO_DIAG_BATCHES"] = "100"
    env["LIO_TRACE_MARKER"] = str(instance / "trace_marker")
    env.pop("LIO_PROFILE", None)
    env.pop("LIO_DIAG_DEFERRED", None)
    if deferred:
        env["LIO_DIAG_DEFERRED"] = "1"
    (instance / "tracing_on").write_text("1")
    subprocess.run([binary, "trace_read_batches", "--ignored", "--nocapture"],
                   env=env, check=True)
    (instance / "tracing_on").write_text("0")
    with output.open("x") as destination:
        destination.write((instance / "trace").read_text())
    print(f"Saved {output}")
finally:
    (instance / "tracing_on").write_text("0")
    instance.rmdir()
