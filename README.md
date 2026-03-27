# Hardpass

`hardpass` is a small Rust CLI for managing local Ubuntu cloud-image VMs with QEMU.

It exists for people who want a simpler, more predictable local VM workflow than Multipass:
- macOS and Linux hosts
- Ubuntu guest images only
- host-native guest architecture only
- QEMU user networking
- stable per-VM SSH port forwarding

## Commands

- `doctor` checks for required local tools and firmware.
- `image prefetch` downloads and verifies a cloud image into the local cache.
- `create` creates a named VM.
- `start` boots a named VM and waits for SSH.
- `stop` gracefully stops a named VM.
- `delete` stops and removes a named VM.
- `list` shows known VMs.
- `info [--json]` prints VM details.
- `ssh` opens an interactive SSH session.
- `exec` runs a remote command over SSH.

## Install

From crates.io:

```bash
cargo install hardpass-vm
```

That installs the `hp` executable.

From the GitHub repository:

```bash
cargo install --git https://github.com/peterdelevoryas/hardpass
```

From a local checkout:

```bash
cargo install --path .
```

That installs `hp` into Cargo's bin directory so the examples below can be run directly.

## Quick Start

```bash
hp doctor
hp image prefetch
hp create dev
hp start dev
hp list
hp info dev
hp ssh dev
hp exec dev -- uname -a
hp stop dev
hp delete dev
```

`create` defaults to Ubuntu `24.04` on the host-native guest architecture. You can override VM size and forwarding when needed:

```bash
hp create test \
  --release 24.04 \
  --cpus 4 \
  --memory-mib 4096 \
  --disk-gib 24 \
  --forward 8080:8080

hp start test
```

If you want to warm the image cache before the first VM boot:

```bash
hp image prefetch
hp image prefetch --release 24.04 --arch amd64
```

Use `info --json` when another tool needs machine-readable state:

```bash
hp info dev --json
```

The JSON payload includes `ssh.alias`, so other tools can discover the SSH alias directly.

## State and SSH

Hardpass stores state under `~/.hardpass` by default. Set `HARDPASS_HOME` if you want a different root.

When using the default `~/.hardpass` root, Hardpass automatically:

- adds `Include ~/.hardpass/ssh/config` to `~/.ssh/config`
- rewrites `~/.hardpass/ssh/config` to match the current VM aliases

Each VM name becomes an SSH alias with the stored loopback port and identity file:

```bash
ssh dev
```

With the default `~/.hardpass` root, `hp create` and `hp delete` keep the alias file up to date automatically.

## Host Requirements

- `qemu-img`
- `qemu-system-x86_64` or `qemu-system-aarch64`
- `ssh`
- `ssh-keygen`
- Linux hosts need `/dev/kvm`; Hardpass does not fall back to TCG
- AArch64 hosts also need discoverable UEFI firmware for QEMU

Run `hp doctor` to confirm the local environment before creating a VM.

## Security Notes

- SSH connections disable host key checking and known-host persistence for loopback convenience.
- The default cloud-init config creates an `ubuntu` user with passwordless sudo.
- Guest networking uses QEMU user networking, not bridged networking.

## Testing

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The real-QEMU integration smoke test is opt-in:

```bash
HARDPASS_REAL_QEMU_TEST=1 cargo test --test library_api_smoke -- --ignored
```

The heavier GitHub Actions e2e test is also opt-in locally on macOS and Linux hosts:

```bash
HARDPASS_REAL_QEMU_TEST=1 cargo test --test e2e_vm_stress -- --ignored --nocapture
```

Both real-QEMU tests use the current `HOME` and the normal Hardpass state at `~/.hardpass`, so they share the default image cache and exercise the same SSH-config behavior a user would get in CI. While they run, you can inspect them with ordinary `cargo run -- list`, `cargo run -- info <name>`, and `cargo run -- ssh <name>`.

In GitHub Actions, the e2e workflow requires `/dev/kvm` and intentionally fails instead of falling back to TCG.

Set `HARDPASS_E2E_PROFILE=stress` to run the 2-VM profile locally.
