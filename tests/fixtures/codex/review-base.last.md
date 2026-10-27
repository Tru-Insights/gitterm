Direct execution confirms incorrect results in both statistics helpers and an exception when a user is absent. The patch introduces three actionable correctness defects.

Full review comments:

- [P2] Include the final complete moving-average window — /scratch/repo/stats.py:9-9
  When `window <= len(values)`, the shortened range omits the final valid window. For example, `moving_average([1, 2, 3], 2)` now returns `[1.5]` instead of `[1.5, 2.5]`, and a window equal to the input length returns no average. Restore the inclusive count of valid window starts.

- [P2] Return None when no user matches — /scratch/repo/stats.py:18-18
  If `users` is empty or no name matches, `matches[0]` raises `IndexError` instead of returning the documented and previously supported `None`. Check whether a match exists before indexing.

- [P2] Correct the starting index for the last n values — /scratch/repo/stats.py:23-23
  The extra subtraction makes `last_n([1, 2, 3], 1)` return `[2, 3]` rather than `[3]`. When `n` equals the input length, the start becomes `-1`, so `last_n([1, 2, 3], 3)` returns only `[3]`. Calculate the start without the extra subtraction and clamp it to zero when necessary.