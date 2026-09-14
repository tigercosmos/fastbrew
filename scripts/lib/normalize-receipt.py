#!/usr/bin/env python3
"""Print an INSTALL_RECEIPT.json with the fields that legitimately vary blanked.

Used by scripts/compat-check.sh to diff the receipts fastbrew and the Ruby
Homebrew write for the same bottle. Install time, the Homebrew version, the
path of the API file, the build machine record and the ordering of
runtime_dependencies are not compatibility facts, so they are normalized away;
everything else has to match.
"""

import json
import sys

VOLATILE = {
    "time",
    "homebrew_version",
    "built_on",
    "source_modified_time",
    "compiler",
    "stdlib",
}


def normalize(receipt):
    out = {}
    for key, value in receipt.items():
        if key in VOLATILE:
            out[key] = "@@VOLATILE@@"
        elif key == "source" and isinstance(value, dict):
            source = dict(value)
            if "path" in source:
                source["path"] = "@@PATH@@"
            if "tap_git_head" in source:
                source["tap_git_head"] = "@@SHA@@"
            out[key] = source
        elif key == "runtime_dependencies" and isinstance(value, list):
            out[key] = sorted(
                (json.dumps(dep, sort_keys=True) for dep in value),
            )
        else:
            out[key] = value
    return out


def main():
    for path in sys.argv[1:]:
        with open(path) as handle:
            receipt = json.load(handle)
        print(json.dumps(normalize(receipt), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
