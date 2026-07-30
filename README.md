# Personal Computer Agent

This repository was initialized from the Code Agent Development Pack.

Start with:

- `00_START_HERE.md`
- `tasks/S0_ENGINEERING_BASELINE.md`

The scaffold is intentionally minimal and does not represent completed product functionality.

## Verification

Run the structural checks when compiler toolchains are unavailable:

```bash
./scripts/verify-structural.sh
```

Run the complete S0 engineering gate before claiming the baseline is ready:

```bash
./scripts/verify-full.sh
```

The full gate requires Rust with `rustfmt` and `clippy`, Swift, pnpm 9.15, and Python 3.9 or newer. On Homebrew systems where `rustup` is keg-only, the script uses `/opt/homebrew/opt/rustup/bin` for that process without modifying shell configuration.
