# Determinism comparison: `determinism_probe_a.log` vs `determinism_probe_c.log`

Produced by `tools/probe/compare_runs.py` from two determinism_probe logs.

- samples in A: 3, in B: 3, common steps: 3
- common step range: 1 .. 200

## Per-lane first divergence

| lane | description | compared | errors | first differing step | A time | B time |
|---|---|---|---|---|---|---|
| `v` | vehicle count | 3 | 0 | identical | | |
| `p` | vehicle positions (1 m) | 3 | 0 | **1** | 1000.000000 | 1000.000000 |
| `e` | edge geometry (0.1 m) | 3 | 0 | identical | | |
| `c` | construction list | 3 | 0 | **1** | 1000.000000 | 1000.000000 |
| `t` | town building counts | 3 | 0 | identical | | |
| `m` | money per player | 3 | 0 | identical | | |
| `n` | people count | 3 | 0 | identical | | |

## First divergence detail

- **p** (vehicle positions (1 m)) first differs at step 1 (A t=1000.000000, B t=1000.000000):
  - A: `1166619529-0173930286`
  - B: `0822546499-0328901283`
- **c** (construction list) first differs at step 1 (A t=1000.000000, B t=1000.000000):
  - A: `1165056400-1092866167`
  - B: `0786217738-0532217293`

