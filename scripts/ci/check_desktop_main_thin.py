#!/usr/bin/env python3
"""Ensure desktop_gui main.rs remains a thin bootstrap entrypoint."""
from pathlib import Path
import re
import sys

main_rs = Path('apps/desktop_gui/src/main.rs')
text = main_rs.read_text()

forbidden = [
    r"struct\s+DesktopGuiApp",
    r"impl\s+eframe::App",
    r"fn\s+render_",
    r"fn\s+decode_",
    r"fn\s+dispatch_",
]

violations = [pat for pat in forbidden if re.search(pat, text)]
if violations:
    print('main.rs contains feature logic; move code into modules before merging.')
    for pat in violations:
        print(f' - matched forbidden pattern: {pat}')
    sys.exit(1)

print('desktop_gui main.rs thin-entrypoint guard passed')
