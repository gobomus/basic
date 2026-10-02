#!/usr/bin/env python3
"""Write a ready-to-run paper config from config/poc.example.toml.

usage: make_poc_config.py OUT_FILE "WALLET1, WALLET2 ..."   (commas, spaces or newlines)

Only the [[leaders]] section is replaced; everything else stays as in the example.
"""
import re
import sys

BASE58 = re.compile(r"^[1-9A-HJ-NP-Za-km-z]{32,44}$")


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    out, raw = sys.argv[1], sys.argv[2]
    wallets = []
    for w in re.split(r"[\s,;]+", raw.strip()):
        if not w:
            continue
        if not BASE58.match(w):
            print(f"not a Solana address: {w!r}", file=sys.stderr)
            return 1
        if w not in wallets:
            wallets.append(w)
    if not wallets:
        print("give at least one leader wallet", file=sys.stderr)
        return 1

    src = open("config/poc.example.toml", encoding="utf-8").read()
    start = src.index("[[leaders]]")
    end = src.index("# ================================================================ infrastructure")
    block = "".join(
        f'[[leaders]]\naddress = "{w}"\nlabel = "leader-{i}"\n\n' for i, w in enumerate(wallets, 1)
    )
    open(out, "w", encoding="utf-8").write(src[:start] + block + src[end:])
    print(f"wrote {out} with {len(wallets)} leader(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
