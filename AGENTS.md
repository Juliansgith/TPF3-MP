# Working on TPF3-MP

How changes are made and released, for people and coding agents alike. Read
this before changing anything. [README.md](README.md) says what the project
is; `docs/` says how it works.

## Branches

Changes move through three long-lived branches, always in this order:

```
feature branch ──> dev ──> acceptance ──> main
 (your work)     (testing) (release     (release: CI/CD drafts
                            candidate)    the release)
```

| branch | holds | may receive | gate to the next |
|---|---|---|---|
| feature (`feat/…`, `fix/…`, `docs/…`) | one change in progress | your commits | `ci` green on the branch |
| `dev` | the integration and testing line | merges of feature branches whose `ci` is green | `ci` green on `dev` |
| `acceptance` | the release candidate | fast-forwards from `dev` | `ci` **and** `acceptance` green on `acceptance`, plus the manual checks below |
| `main` | what is released | fast-forwards from `acceptance` | the `release` workflow drafts the release |

Rules:

- **Never commit to `dev`, `acceptance` or `main` directly.** Work on a
  feature branch cut from `dev`, and merge it in once its `ci` is green.
- **Promote by fast-forward only.** `acceptance` and `main` never get
  commits of their own, so each always equals an earlier state of the
  branch before it, and a release is exactly a commit that passed every
  check. Force-pushes to these branches are never allowed.
- **Do not skip a stage.** Nothing reaches `main` without passing
  `acceptance` first, however small the change. A fix found in
  acceptance goes in on a feature branch, then through `dev` again.
- **A red check stops promotion.** Fix it on a feature branch; never
  promote around it or disable the check.
- Old milestone branches (`m0-foundations`, `m1-core`) are history. New
  work starts from `dev`.

## The checks

- **`ci`** (`.github/workflows/ci.yml`) runs on every push and pull request,
  on Windows, Linux and macOS:
  - `cargo fmt --check`;
  - `clippy -D warnings`;
  - the whole test suite;
  - release builds of the binaries players and servers run;
  - the server container image.
- **`acceptance`** (`.github/workflows/acceptance.yml`) runs on pushes to
  `acceptance`. It runs optimized load tests on all three platforms, each
  failing on any failed bot or diverged replica:
  - rooms of bots at the room's pace;
  - a bad network (latency, jitter, loss);
  - every bot through the WebSocket tunnel;
  - rooms logged and compacted under load;
  - a 15-minute soak on Linux.
- **Manual acceptance**, before promoting to `main`, for changes they
  touch:
  - the launcher, used by hand against a local server: connect, create a
    room, join by invite, play (see [docs/PLAYING.md](docs/PLAYING.md));
  - a server upgrade that keeps running games (see "Upgrades" in
    [docs/OPERATIONS.md](docs/OPERATIONS.md)), when the log format, the
    protocol or persistence changed;
  - once the game is out: a real game on each platform
    ([docs/DAY_ONE.md](docs/DAY_ONE.md)).
- **`release`** (`.github/workflows/release.yml`) runs on pushes to `main`.
  It builds the packages for every platform and attaches them to a draft
  release `v<version>`, the version in `Cargo.toml`. A person reviews the
  draft and publishes it, which creates the tag. Once a version is
  published, `main` needs a version bump before it releases again: bump it
  on a feature branch like any change.

## Promoting

```bash
# a feature into dev, once its ci run is green
git switch dev && git pull --ff-only
git merge --no-ff feat/my-change && git push origin dev

# dev into acceptance, once ci on dev is green
git switch acceptance && git pull --ff-only
git merge --ff-only dev && git push origin acceptance

# acceptance into main, once ci, acceptance and the manual checks are green
git switch main && git pull --ff-only
git merge --ff-only acceptance && git push origin main
```

Check a branch's runs with `gh run list --branch <branch>`. Promote only
the exact commit those runs tested: if the branch moved since, wait for the
new runs.

## Rules for every change

- **Rust everywhere; fail closed.** When unsure, refuse, disconnect or set
  aside. Never guess, and never damage a player's game or a room's log.
  Decisions and their reasons are in [docs/DECISIONS.md](docs/DECISIONS.md);
  record a new decision there instead of rewriting an old one.
- **Tests with the change.** A behaviour change comes with a test that fails
  without it. Run `cargo fmt --all`, `cargo clippy --workspace --all-targets
  -- -D warnings` and `cargo test --workspace` before pushing.
- **Docs with the change.** Update the doc that describes what changed
  (`docs/PROTOCOL.md`, `docs/OPERATIONS.md`, `docs/PLAYING.md`, …) in the
  same change. Bump `PROTOCOL_VERSION`, `BRIDGE_VERSION` or the log's
  `FORMAT_VERSION` when their format changes.
- **Commit messages** say what changed for players or operators, in the
  imperative ("Let each room's host choose its rules"), with the details in
  the body.
- **Other repositories are read-only.** The sibling TPF2 projects (`tf2mod`,
  `tf2mp-relay`, `tpf2-multiplayer`) and the game install are inputs; never
  modify them. Never launch or modify the game from automation. Credit code
  or test vectors taken from them in the file that uses them.
- **Secrets stay out of the repository**: keys, certificates and
  `invite.key` included.
