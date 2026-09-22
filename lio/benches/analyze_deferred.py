"""Summarize paired benchmark CSVs; only Python's standard library is needed."""
import csv
import math
import random
import statistics
import sys
from collections import defaultdict

for filename in sys.argv[1:]:
    groups = defaultdict(list)
    with open(filename, newline="") as source:
        for row in csv.DictReader(source):
            groups[row["workload"]].append(
                (float(row["baseline_ns"]), float(row["deferred_ns"]))
            )
    print(filename)
    for name, pairs in groups.items():
        rng = random.Random(1337)
        before = statistics.mean(b for b, _ in pairs)
        after = statistics.mean(a for _, a in pairs)
        change = 100 * (1 - after / before)
        # Resample whole pairs to preserve the shared measurement conditions.
        bootstrap = []
        for _ in range(10000):
            chosen = rng.choices(pairs, k=len(pairs))
            bootstrap.append(100 * (1 - sum(a for _, a in chosen) /
                                    sum(b for b, _ in chosen)))
        bootstrap.sort()
        non_ties = [(b, a) for b, a in pairs if b != a]
        n = len(non_ties)
        wins = sum(a < b for b, a in non_ties)
        tail = min(wins, n - wins)
        sign_p = min(1, 2 * sum(math.comb(n, k) for k in range(tail + 1)) / 2**n)
        print(f"{name:16} n={len(pairs):2} before={before / 1000:.3f} us "
              f"after={after / 1000:.3f} us reduction={change:.2f}% "
              f"95% CI=[{bootstrap[250]:.2f}, {bootstrap[9750]:.2f}] "
              f"wins={wins}/{n} sign_p={sign_p:.3g}")
