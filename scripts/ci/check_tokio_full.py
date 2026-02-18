#!/usr/bin/env python3
"""Fail CI if tokio/full is enabled in non-runtime crates."""

from __future__ import annotations

import re
from pathlib import Path

RUNTIME_CRATES = {
    Path("crates/server"),
    Path("apps/desktop"),
    Path("apps/desktop_gui"),
    Path("apps/tools"),
}

TOKIO_FEATURES_RE = re.compile(
    r"tokio\s*=\s*\{[^}]*features\s*=\s*\[([^\]]*)\][^}]*\}", re.DOTALL
)


def tokio_has_full(manifest: Path) -> bool:
    text = manifest.read_text()
    for m in TOKIO_FEATURES_RE.finditer(text):
        features = m.group(1)
        if '"full"' in features or "'full'" in features:
            return True
    return False


def main() -> int:
    root = Path(__file__).resolve().parents[2]
    failures: list[str] = []

    for manifest in sorted(root.glob("**/Cargo.toml")):
        rel = manifest.relative_to(root)
        if rel == Path("Cargo.toml"):
            if tokio_has_full(manifest):
                failures.append(str(rel))
            continue

        if rel.parent in RUNTIME_CRATES:
            continue

        if tokio_has_full(manifest):
            failures.append(str(rel))

    if failures:
        print("tokio/full is forbidden outside runtime crates. Found in:")
        for item in failures:
            print(f"  - {item}")
        return 1

    print("tokio/full check passed for non-runtime crates.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
