"""Regenerate the public synthetic 20,000-line addition fixture."""
from pathlib import Path

patch = (
    "diff --git a/large.rs b/large.rs\n"
    "new file mode 100644\n"
    "--- /dev/null\n"
    "+++ b/large.rs\n"
    "@@ -0,0 +1,20000 @@\n"
    + "".join(f"+pub const ROW_{i:05}: usize = {i};\n" for i in range(20_000))
)
(Path(__file__).parent / "showcase" / "large.patch").write_text(patch)
