# clickflash

A black window that flashes white for one frame on every click or key press, for measuring click-to-photon
latency.

    clickflash --fullscreen
    clickflash --log flashes.jsonl     # every click and flash, with timestamps

`clickflash --help` for the rest. Q or Escape quits.

## Prereqs

- Vulkan 1.3

## Installing

On Linux, grab the release tarball, check it against its checksum, and put the binary in `~/.local/bin`:

    name=clickflash-linux-$(uname -m)
    curl -fLO https://github.com/porkloin/clickflash/releases/latest/download/$name.tar.gz
    curl -fLO https://github.com/porkloin/clickflash/releases/latest/download/SHA256SUMS
    sha256sum -c --ignore-missing SHA256SUMS
    tar xzf $name.tar.gz
    install -Dm755 $name/clickflash ~/.local/bin/clickflash

Or with Homebrew:

    brew install porkloin/tap/clickflash

Note: on older Linux distros where Homebrew uses its own glibc, it may not be able to load your GPU
driver. In that case, use a binary from the [releases page](https://github.com/porkloin/clickflash/releases) instead.

## Building

    cargo build --release

GPL-3.0
