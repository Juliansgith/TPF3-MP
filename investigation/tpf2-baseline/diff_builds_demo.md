# Build diff: `TransportFever2.exe` -> `TransportFever3.exe (synthetic day-two patch)`

Produced by `tools/re/diff_builds.py`. Functions are matched by recovered name + source file across the two symbol maps.

| | old | new |
|---|---|---|
| binary | `TransportFever2.exe` | `TransportFever3.exe (synthetic day-two patch)` |
| arch | x86_64 | x86_64 |
| named functions | 24200 | 24200 |

## Summary

- unchanged (same name, size, RVA): **0**
- moved (same size, new RVA): **24196**
- resized (body changed -- re-verify signatures): **3**
- appeared (new in newer build): **1**
- disappeared (gone from newer build): **1**
- most common RVA delta among moved functions: 0x4000 (24196 functions) -- a uniform shift of this size is an ordinary relayout.

## Resized functions (re-verify hook signatures)

| function | source | old RVA | new RVA | old size | new size |
|---|---|---|---|---|---|
| `AchievementRep::Add` | game\achievementrep.cpp | 0x98510 | 0x9C510 | 250 | 282 |
| `AchievementRep::GetIndex` | game\achievementrep.cpp | 0x98610 | 0x9C610 | 232 | 264 |
| `AchievementRep::SetNameAndDesc` | game\achievementrep.cpp | 0x98700 | 0x9C700 | 626 | 658 |

## Appeared (new signatures in the newer build)

| function | source | RVA | size |
|---|---|---|---|
| `make_cmd::NewDayTwoCommand` | game\command\make_command.cpp | 0x9000000 | 80 |

## Disappeared (gone from the newer build)

| function | source | RVA | size |
|---|---|---|---|
| `CGame::RunGameSimLoop` | game\game.cpp | 0x1184D0 | 1164 |

## Moved only (same size, new address)

24196 functions moved with an unchanged body. A hook re-derives their address from the symbol map; the byte signature should still match.

