# fedoravm

[![CI / Release](https://github.com/YOUR_GITHUB_USERNAME/fedoravm/actions/workflows/release.yml/badge.svg)](https://github.com/YOUR_GITHUB_USERNAME/fedoravm/actions/workflows/release.yml)

A small Rust CLI for creating and managing Fedora KDE Plasma virtual machines with QEMU/KVM.

`fedoravm` is designed for Linux/x86_64 hosts and focuses on a simple workflow:

- automatically discover the current stable Fedora KDE Plasma Desktop release
- download and SHA-256 verify the official Fedora ISO
- create a `qcow2` disk and UEFI firmware variables
- use `virtio-gpu-gl` with `blob=true` and `venus=true` for GPU acceleration
- expose host directories through `virtiofs`
- create disposable VMs with `--temp` / `-temp`
- configure RAM, vCPUs, virtual disk size, and GPU host memory
- manage VMs with `start`, `install`, `stop`, `delete`, `list`, and `doctor`

> **Status:** Experimental. The project intentionally wraps QEMU directly instead of using libvirt/virt-manager.

## Requirements

### Host

- Linux x86_64
- QEMU with KVM support
- `qemu-img`
- `virtiofsd` when using `--share`
- OVMF/EDK2 UEFI firmware
- a working Vulkan + virglrenderer stack for Venus GPU acceleration

On Fedora, the base packages are:

```bash
sudo dnf install qemu-system-x86-core qemu-img virtiofsd edk2-ovmf
```

For Venus acceleration, the host kernel, QEMU, Vulkan implementation, and virglrenderer stack must support the required features. Run:

```bash
fedoravm doctor
```

to inspect the most important host capabilities.

## Installation

### From a GitHub Release

Download the latest Linux x86_64 archive from the repository's **Releases** page, extract it, and install the binary:

```bash
tar -xzf fedoravm-linux-x86_64.tar.gz
install -Dm755 fedoravm ~/.local/bin/fedoravm
```

### From source

Install a current stable Rust toolchain, then:

```bash
cargo build --release
install -Dm755 target/release/fedoravm ~/.local/bin/fedoravm
```

## Usage

Create a normal VM:

```bash
fedoravm create kde-dev
```

Create a VM with a host directory shared through virtiofs:

```bash
fedoravm create kde-dev --share "$HOME/projects"
```

Multiple shares are supported:

```bash
fedoravm create kde-dev \
  --share "$HOME/projects" \
  --share "$HOME/Documents"
```

Create a disposable VM that is removed when QEMU exits:

```bash
fedoravm create -temp
```

The generated temporary VM name is automatic. You can also pass explicit resources:

```bash
fedoravm create kde-dev \
  --ram 12G \
  --cpus 6 \
  --disk 100G \
  --gpu-memory 8G
```

### VM management

```text
fedoravm start NAME      Start an installed VM
fedoravm install NAME    Boot the Fedora installer once
fedoravm stop NAME       Stop a running VM
fedoravm delete NAME     Delete a VM and its local state
fedoravm list             List known VMs
fedoravm doctor           Check host/QEMU/UEFI capabilities
```

Use a custom data directory with:

```bash
fedoravm --data-dir /path/to/fedoravm-data list
```

or:

```bash
FEDORAVM_DATA_DIR=/path/to/fedoravm-data fedoravm list
```

## Fedora ISO handling

`fedoravm create` queries the official Fedora KDE download page, determines the current stable release, downloads the corresponding x86_64 ISO, and verifies its SHA-256 hash against the Fedora `CHECKSUM` file before using it.

The ISO is cached below the configured data directory so subsequent VM creations can reuse the same image.

## GPU acceleration: virtio-gpu Venus

The default display device is configured as:

```text
virtio-gpu-gl,hostmem=4G,blob=true,venus=true
```

The exact host-memory window is configurable with `--gpu-memory`.

This is intended to enable the Vulkan-based Venus path inside the guest. It is not a fallback software-rendering mode: when the required host capabilities are missing, `fedoravm` reports the problem instead of silently claiming that Venus is active.

The guest should use a sufficiently recent Fedora KDE Plasma stack with Mesa/virtio-gpu support.

## Host shares: virtiofs

`--share PATH` starts `virtiofsd` and presents the directory to the guest with a `vhost-user-fs-pci` device. Multiple shares receive tags `share0`, `share1`, and so on.

Inside the Fedora guest, mount a share with:

```bash
sudo mkdir -p /mnt/host-projects
sudo mount -t virtiofs share0 /mnt/host-projects
```

For a second share:

```bash
sudo mkdir -p /mnt/host-documents
sudo mount -t virtiofs share1 /mnt/host-documents
```

When one or more shares are configured, QEMU receives a shared memory backend because the vhost-user filesystem device depends on shared guest memory.

## Temporary VMs

`--temp` (and the requested shorthand `-temp`) creates a disposable VM. The VM directory, virtual disk, and UEFI variables are removed after QEMU exits.

This mode is useful for short-lived test environments. Do not use it for work you need to preserve.

## Data layout

By default, VM state is stored below:

```text
~/.local/share/fedoravm/
├── cache/
│   └── <Fedora KDE ISO>
└── <vm-name>/
    ├── vm.json
    ├── <vm-name>.qcow2
    └── OVMF_VARS.fd
```

The exact location can be changed with `--data-dir` or `FEDORAVM_DATA_DIR`.

## Development

Format and check the project locally:

```bash
cargo fmt --check
cargo check
cargo clippy -- -D warnings
```

## Automated releases

Every push to `main` or `master` runs the GitHub Actions workflow in `.github/workflows/release.yml`.

The workflow:

1. checks out the commit
2. installs the stable Rust toolchain
3. runs formatting and compile checks
4. builds the release binary
5. packages `fedoravm` as a Linux x86_64 tarball
6. creates a SHA-256 checksum
7. creates a GitHub Release for that commit
8. uploads the binary archive and checksum as release assets

Release tags are generated from the package version, workflow run number, and commit SHA, for example:

```text
build-0.1.0-42-a1b2c3d4e5f6
```

> Update the badge URL at the top of this README if the repository is not named `fedoravm` or uses a different GitHub owner.

## License

Copyright © 2026 the fedoravm contributors.

This project is licensed under the **GNU Affero General Public License v3.0 or later**. See [LICENSE](LICENSE) for the complete license text.

SPDX identifier: `AGPL-3.0-or-later`.
