# cargo-auto-bump

Mass-production helper for Rust dependency refresh and rebuild.

## What it does

`cargo-auto-bump` runs this fixed flow:

1. Resolve project root from `--manifest-or-dir`.
2. Run `cargo update`.
3. Scan duplicate dependency trees (`cargo tree --all-targets -d`).
4. Aggressively reconcile duplicates (default enabled):
   - For each duplicated crate root, fetch latest version via `cargo info <crate>`.
   - Attempt `cargo update --precise <latest> --package <crate>`.
   - Retry with `cargo update --package <crate>` as fallback.
   - Repeat for `--aggressive-passes` passes (default `3`) or until no progress.
5. Run build (`cargo build` with optional `--target` and profile arguments).
6. Optionally copy output artifact to `--copy-to`.

Aggressive reconciliation is non-blocking: if a crate cannot be forced to one version, the run logs it and continues.

## Install

```bash
cd /root/mic-volume-fix/tools/cargo-auto-bump
./install.sh
```

## Run

```bash
cargo-auto-bump \
  --manifest-or-dir /path/to/repo/Cargo.toml \
  --workspace \
  --target x86_64-pc-windows-msvc \
  --copy-to /mnt/c/code/mic-volume-fix-bumped.exe
```

Or via the local wrapper:

```bash
/path/to/mic-volume-fix/tools/run-auto-bump.sh \
  --manifest-or-dir /path/to/repo/Cargo.toml
```

## Options

- `--manifest-or-dir <path>`: path to `Cargo.toml` or project directory.
- `--target <triple>`: build target triple.
- `--profile <name>`: `release` (default), `debug`, or custom profile.
- `--workspace`: include workspace members in update/tree/build.
- `--aggressive-passes <n>`: number of dedupe passes (default `3`).
  - set to `0` to skip aggressive reconciling.
- `--copy-to <path>`: copy built artifact to this destination.
- `--no-copy`: disable artifact copy.
- `--dry-run`: print planned commands without running.

## Notes

- This tool does not edit `Cargo.toml` directly.
- It is intentionally strict in flow and lean in surface area for CI/release use.
