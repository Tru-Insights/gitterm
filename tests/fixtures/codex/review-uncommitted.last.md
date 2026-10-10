The change introduces a confirmed regression: unsuccessful user lookups now raise IndexError rather than returning None.

Review comment:

- [P2] Return None when no user matches — /scratch/repo/stats.py:18-18
  When `users` is empty or no user has the requested name, `matches[0]` raises `IndexError` instead of returning `None`. This breaks both the previous behavior and the documented contract. Guard the empty result before indexing.