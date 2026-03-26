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
- `create` creates a named VM.
- `start` boots a named VM and waits for SSH.
- `stop` gracefully stops a named VM.
- `delete` stops and removes a named VM.
- `list` shows known VMs.
- `info [--json]` prints VM details.
- `ssh-config install` adds the managed Hardpass include to `~/.ssh/config`.
- `ssh-config sync` rewrites the managed Hardpass host aliases.
- `ssh` opens an interactive SSH session.
- `exec` runs a remote command over SSH.

## Quick Start

```bash
cargo run -- doctor
cargo run -- create dev
cargo run -- start dev
cargo run -- list
cargo run -- info dev
cargo run -- ssh dev
cargo run -- exec dev -- uname -a
cargo run -- ssh-config install
cargo run -- ssh-config sync
cargo run -- stop dev
cargo run -- delete dev
```

`create` defaults to Ubuntu `24.04` on the host-native guest architecture. You can override VM size and forwarding when needed:

```bash
cargo run -- create test \
  --release 24.04 \
  --cpus 4 \
  --memory-mib 4096 \
  --disk-gib 24 \
  --forward 8080:8080

cargo run -- start test
```

Use `info --json` when another tool needs machine-readable state:

```bash
cargo run -- info dev --json
```

The JSON payload includes `ssh.alias`, so other tools can discover the SSH alias directly.

## State and SSH

Hardpass stores state under `~/.hardpass` by default. Set `HARDPASS_HOME` if you want a different root.

Install the one-time include block, then sync the current VM aliases:

```bash
cargo run -- ssh-config install
cargo run -- ssh-config sync
```

Each VM name becomes an SSH alias with the stored loopback port and identity file:

```bash
ssh dev
```

If the managed include is installed, `hardpass create` and `hardpass delete` keep the alias file up to date automatically for the default `~/.hardpass` root.

## Host Requirements

- `qemu-img`
- `qemu-system-x86_64` or `qemu-system-aarch64`
- `ssh`
- `ssh-keygen`
- AArch64 hosts also need discoverable UEFI firmware for QEMU

Run `hardpass doctor` to confirm the local environment before creating a VM.

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
