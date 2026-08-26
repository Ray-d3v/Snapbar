from __future__ import annotations

import base64
import gzip
from pathlib import Path

MAPPING = {
    "tools/titlebar-payload/app.rs.gz.b64": "src/app.rs",
    "tools/titlebar-payload/overlay.rs.gz.b64": "src/overlay.rs",
    "tools/titlebar-payload/README.md.gz.b64": "README.md",
    "tools/titlebar-payload/AGENTS.md.gz.b64": "AGENTS.md",
    "tools/titlebar-payload/OVERLAY_BEHAVIOR.md.gz.b64": "docs/OVERLAY_BEHAVIOR.md",
}


def main() -> None:
    for source_name, target_name in MAPPING.items():
        source = Path(source_name)
        target = Path(target_name)
        encoded = source.read_text(encoding="utf-8").strip()
        decoded = gzip.decompress(base64.b64decode(encoded, validate=True))
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(decoded)


if __name__ == "__main__":
    main()
