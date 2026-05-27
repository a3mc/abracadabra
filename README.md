<div align="center">

![abracadabra](assets/repo_header.png)

**Solana Alpenglow validator log analyzer — terminal UI**

[![ci](https://github.com/a3mc/abracadabra/actions/workflows/ci.yml/badge.svg)](https://github.com/a3mc/abracadabra/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![rustc](https://img.shields.io/badge/rustc-1.90%2B-orange.svg)](https://www.rust-lang.org)

</div>

---

## Build

```sh
cargo build --release
./target/release/abracadabra path/to/validator.log
```

Or grab a prebuilt binary from [Releases](https://github.com/a3mc/abracadabra/releases) — Linux `gnu` (glibc ≥ 2.35) or fully-static `musl`.

## Tour

### Overview

![overview](assets/overview_tab.png)

### Time series

![time-series](assets/time_series_tab.png)

### Windows

![windows](assets/windows_tab.png)

### Slots

![slots](assets/slots_tab.png)

### Leader timeouts

![leader-timeouts](assets/leader_timeouts_tab.png)

### Alerts

![alerts](assets/alerts_tab.png)

## Keys

| | |
|---|---|
| `1`–`6` / `Tab` | Switch tabs |
| `j` / `k` · `PgUp` / `PgDn` · `g` / `G` | Scroll |
| `t` `n` `p` | Slots filter — TCL / S2N / S2S |
| `l` `f` `x` `s` | Slots filter — leader / fast / slow / skipped |
| `c` | Slots — clear filters |
| `y` | Alerts — yank to `/tmp/abracadabra-yank-N.txt` |
| `q` / `Esc` | Quit |

## Flags

```
--bucket <DUR>   Time-series bucket size  (default 10m, range 1m..=24h)
--text           Non-interactive summary instead of the TUI
--version        Print version
--help           Print full help
```

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.

Built by [ART3MIS.CLOUD](https://art3mis.cloud).
