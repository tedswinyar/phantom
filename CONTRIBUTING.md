# Contributing

This is a personal project template; contributions are by arrangement rather
than an open PR queue. If something here is useful to you and you want to
change it, open an issue (or reach the author directly) before writing code.

**Community standards**: This project follows the [Contributor Covenant v2.1](https://www.contributor-covenant.org/version/2/1/code_of_conduct/)
Code of Conduct (see [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)). Be
respectful, assume good intent.

If we have agreed on a change:

- `./scripts/verify.sh` green is the bar — the pre-push hook enforces it.
- Tests are mutation-proof and mock only at the network boundary; parser
  tests start from raw fixture bytes. Match the surrounding code's style.
- Wire-format changes additionally require updating `tests/fixtures/`, the
  OPE contract docs, and `open-prompt-edition/VERSION` in the same push.
