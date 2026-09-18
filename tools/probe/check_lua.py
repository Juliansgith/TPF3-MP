#!/usr/bin/env python3
"""check_lua.py -- syntax-check the probe Lua with a real interpreter.

The determinism and script-API probes must load in the game's Lua VM (5.2 on
TPF2). This compiles every .lua under the probe mods with an actual Lua 5.2 (via
the `lupa` package -- no system settings touched, no game launched), so a syntax
slip is caught here instead of as a silent mod-load failure in the game. Lua 5.1
is also tried where available, because TPF2's engine has 5.1-era quirks; a 5.1
failure is reported as a warning, not an error, since the target is 5.2.

    python tools/probe/check_lua.py [paths...]      # default: tools/probe

Exit code 0 when every file compiles under Lua 5.2, 1 otherwise.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path


def _runtime(module_name: str):
    import importlib
    mod = importlib.import_module("lupa." + module_name)
    return mod.LuaRuntime()


def compile_check(rt, source: str, chunkname: str) -> tuple[bool, str]:
    """Return (ok, message). Uses the VM's own loader so it is a real parse.

    The Lua side always returns exactly two values (a boolean and a string) so
    the result marshals back through lupa as a stable 2-tuple regardless of
    whether `load` succeeded (1 value) or failed (2 values).
    """
    loader = rt.eval(
        "function(src, name)\n"
        "  local mk = (_VERSION == 'Lua 5.1') and loadstring or load\n"
        "  local f, e = mk(src, name)\n"
        "  if f then return true, 'ok' else return false, tostring(e) end\n"
        "end")
    try:
        ok, msg = loader(source, "@" + chunkname)
    except Exception as exc:  # a raised LuaError also means a parse failure
        return False, str(exc)
    return bool(ok), str(msg)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Syntax-check probe Lua with a real Lua 5.2 (lupa).")
    ap.add_argument("paths", nargs="*", type=Path,
                    help="files or directories (default: the tools/probe tree)")
    ap.add_argument("--also-51", action="store_true",
                    help="also report Lua 5.1 results (default: on if available)")
    args = ap.parse_args(argv)

    roots = args.paths or [Path(__file__).resolve().parent]
    files: list[Path] = []
    for r in roots:
        if r.is_dir():
            files.extend(sorted(r.rglob("*.lua")))
        elif r.is_file() and r.suffix == ".lua":
            files.append(r)
    if not files:
        print("check_lua: no .lua files found under %s" % ", ".join(str(r) for r in roots),
              file=sys.stderr)
        return 1

    try:
        rt52 = _runtime("lua52")
    except Exception as exc:
        print("check_lua: need lupa with Lua 5.2 (pip install lupa): %s" % exc, file=sys.stderr)
        return 2
    try:
        rt51 = _runtime("lua51")
    except Exception:
        rt51 = None

    ok_all = True
    for f in files:
        src = f.read_text(encoding="utf-8")
        ok, msg = compile_check(rt52, src, f.name)
        status = "OK  " if ok else "FAIL"
        extra = ""
        if rt51 is not None:
            ok51, msg51 = compile_check(rt51, src, f.name)
            extra = "  [5.1 %s]" % ("ok" if ok51 else ("WARN: " + msg51.splitlines()[0]))
        print("%s  %-70s (5.2)%s" % (status, str(f), extra))
        if not ok:
            ok_all = False
            print("      %s" % msg.replace("\n", " "))
    print("\n%s: %d file(s) checked under Lua 5.2" % ("PASS" if ok_all else "FAILURES", len(files)))
    return 0 if ok_all else 1


if __name__ == "__main__":
    raise SystemExit(main())
