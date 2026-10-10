The commit introduces two reproducible slice/loop-bound errors: moving_average drops a valid result, and last_n returns incorrect suffixes.

Full review comments:

- [P2] Restore the final valid moving-average window — /scratch/repo/stats.py:9-9
  When `len(values) >= window`, the new loop bound omits the final valid window. For example, `moving_average([1, 2, 3], 2)` now returns `[1.5]` instead of `[1.5, 2.5]`, and a window equal to the input length returns no average. Restore the `+ 1` bound to preserve the documented behavior.

- [P2] Correct the last_n slice boundary — /scratch/repo/stats.py:25-25
  For ordinary positive counts, this slice returns one extra value: `last_n([1, 2, 3], 1)` returns `[2, 3]`. When `n` equals the input length, the negative start instead returns only the final value, and `n=0` also returns that value rather than an empty result. Use a correctly bounded suffix slice and handle zero explicitly.