# vmforge

[![Build and Release](https://github.com/YOUR_GITHUB_USERNAME/vmforge/actions/workflows/release.yml/badge.svg)](https://github.com/YOUR_GITHUB_USERNAME/vmforge/actions/workflows/release.yml)

A small Rust CLI for creating and managing Linux virtual machines with QEMU/KVM.

`vmforge` keeps the simple workflow of the original project, but is no longer Fedora-specific. It can bootstrap the current official installer ISO for:

- Fedora KDE Plasma Desktop
- CachyOS Desktop
- Arch Linux
- Debian stable
- Ubuntu Desktop LTS

It also supports persistent `virtiofs` host shares, disposable VMs, configurable RAM/CPU/disk/GPU memory, and a Venus/OpenGL graphics path for the installed guest.

> **Status:** Experimental. `vmforge` is intentionally a thin wrapper around QEMU rather than a libvirt/virt-manager frontend.

## Requirements

### Host

- Linux x86_64
- QEMU with KVM support (`qemu-system-x86_64`)
- `qemu-img`
- OVMF/EDK2 UEFI firmware
- `virtiofsd` when using `--share`
- A working Vulkan + virglrenderer stack for Venus acceleration

`vmforge` searches common distribution-specific locations for `virtiofsd` and OVMF. This includes `/usr/lib/virtiofsd` and `/usr/share/edk2/x64/OVMF_CODE.4m.fd`, which are common on Arch/CachyOS systems.

Run:

```bash
vmforge doctor
```

to inspect host-side capabilities.

## Installation

### From a GitHub Release

Download the latest Linux x86_64 archive from the **Releases** page:

```bash
tar -xzf vmforge-linux-x86_64.tar.gz
install -Dm755 vmforge ~/.local/bin/vmforge
```

### From source

Install a current stable Rust toolchain, then:

```bash
cargo build --release
install -Dm755 target/release/vmforge ~/.local/bin/vmforge
```

## Basic usage

The distro selector defaults to Fedora KDE for backwards compatibility:

```bash
vmforge create kde-dev
```

Explicit distro flags are supported in both GNU-style and the requested single-dash spelling:

```bash
vmforge create kde-dev --fedora
vmforge create kde-dev -fedora

vmforge create cachy --cachyos
vmforge create archbox -arch
vmforge create debian-dev --debian
vmforge create ubuntu-dev -ubuntu
```

A generic value selector is also available:

```bash
vmforge create kde-dev --distro fedora
vmforge create ubuntu-dev --distro ubuntu
```

Only one distro selector may be used at a time.

## VM creation options

```bash
vmforge create NAME \
  --fedora \
  --ram 12G \
  --cpus 6 \
  --disk 100G \
  --gpu-memory 8G
```

Create a disposable VM that is deleted when QEMU exits:

```bash
vmforge create -temp
```

A disposable VM may still use an explicit distro:

```bash
vmforge create -temp -ubuntu
```

## Host directory shares

Add a host directory during VM creation:

```bash
vmforge create kde-dev --fedora --share "$HOME/Documents"
```

Multiple shares can be specified:

```bash
vmforge create kde-dev \
  --share "$HOME/projects" \
  --share "$HOME/Documents"
```

Shares can also be added to an existing, stopped VM:

```bash
vmforge share add kde-dev "$HOME/Documents"
vmforge share list kde-dev
vmforge share remove kde-dev 0
```

A share is presented to the guest as `share0`, `share1`, and so on. For example:

```bash
sudo mkdir -p /mnt/host-documents
sudo mount -t virtiofs share0 /mnt/host-documents
```

The VM must be stopped while changing its persistent share configuration.

`vmforge` starts a detached supervisor for each VM. It keeps QEMU and every `virtiofsd` process alive after the original terminal closes, launches the VNC viewer, and performs socket cleanup when the VM stops. The supervisor writes QEMU and VM diagnostics to `~/.local/share/vmforge/NAME/vmforge.log` (or the configured data directory). `vmforge start NAME` returns once QEMU's VNC endpoint is ready.

`vmforge` starts one `virtiofsd` process per share and waits until the Unix socket is actually accepting connections before starting QEMU. If a share backend fails, its diagnostic log is written to the VM state directory as `virtiofs-N.log`.

For normal VM boots, `vmforge` automatically mounts configured shares through the QEMU Guest Agent under `/mnt/vmforge/share0`, `/mnt/vmforge/share1`, and so on.

Verify them inside the guest with:

```bash
mount | grep virtiofs
touch /mnt/vmforge/share0/vmforge-test
```

For live installers or guests without `qemu-guest-agent`, the device is still exposed as `share0`, `share1`, etc. and can be mounted manually:

```bash
sudo mkdir -p /mnt/host-documents
sudo mount -t virtiofs share0 /mnt/host-documents
```


## QEMU Guest Agent

`vmforge` automatically exposes the standard QEMU Guest Agent channel to every VM:

```text
org.qemu.guest_agent.0
```

This creates a host-side Unix socket at `qga.sock` inside the VM state directory and connects it through `virtio-serial`. The socket is removed automatically when QEMU exits.

Inside Fedora, install and enable the guest agent once:

```bash
sudo dnf install qemu-guest-agent
sudo systemctl enable --now qemu-guest-agent
```

Verify that the virtio channel is present:

```bash
ls -l /dev/virtio-ports/org.qemu.guest_agent.0
systemctl status qemu-guest-agent
```

For Arch/CachyOS guests, the package is commonly named `qemu-guest-agent` as well:

```bash
sudo pacman -S qemu-guest-agent
sudo systemctl enable --now qemu-guest-agent
```

The Guest Agent channel is available regardless of whether Venus graphics or `--graphics safe` is used.

## Graphics modes

The default installed-guest graphics path is Venus:

```bash
vmforge start kde-dev
```

For a compatibility/diagnostic 2D boot:

```bash
vmforge start kde-dev --graphics safe
```

To use accelerated graphics while booting the installer, explicitly request it:

```bash
vmforge create kde-dev --fedora --installer-3d
```

The normal installer path intentionally disables 3D acceleration for maximum compatibility with live installers. After installation, the configured graphics mode is used normally.

## Commands

```text
vmforge create NAME [options]
vmforge start NAME [--graphics venus|safe]  # starts in background
vmforge install NAME [--graphics venus|safe] # installer also runs in background
vmforge install NAME [--graphics venus|safe]
vmforge connect NAME
vmforge stop NAME
vmforge delete NAME
vmforge list
vmforge doctor
vmforge share add NAME PATH
vmforge share list NAME
vmforge share remove NAME INDEX
```

A custom state directory can be selected with:

```bash
vmforge --data-dir /path/to/vmforge-data list
```

or:

```bash
VMFORGE_DATA_DIR=/path/to/vmforge-data vmforge list
```

## ISO discovery and verification

`vmforge` resolves the latest installer media at creation time and verifies the downloaded ISO with a SHA-256 checksum before attaching it to the VM.

The providers use their official download infrastructure:

- Fedora KDE: Fedora's official KDE download page and release mirror
- CachyOS: the official CachyOS desktop ISO mirror
- Arch Linux: the official Arch download page plus an Arch mirror's `latest` ISO tree
- Debian: Debian's official download page plus the Debian CD mirror
- Ubuntu: Ubuntu's official Desktop download page plus the Ubuntu release archive

ISOs are cached below the configured vmforge data directory, separated by distro.

## QEMU / virtiofs details

When no shares are configured, QEMU gets the normal `-m RAM` option.

When one or more `virtiofs` shares are configured, `vmforge` also creates a shared `memory-backend-memfd` and keeps its size synchronized with the VM RAM:

```text
-m 8G
-object memory-backend-memfd,id=mem,size=8G,share=on
-numa node,memdev=mem
```

This avoids QEMU's NUMA mismatch error when using `vhost-user-fs-pci`.

## Existing VMs from the old Fedora-only name

The binary is now called `vmforge`. To make the rename less disruptive, the default data directory is `~/.local/share/vmforge`, but `vmforge` also checks the old `~/.local/share/fedoravm` directory when loading/listing an existing VM. Old configurations without a `distro` field are treated as Fedora configurations.

## Distribution notes

The distro flags select the installer image; they do not attempt an unattended OS installation. The initial boot remains interactive so users can choose partitioning, accounts, locale, desktop choices, and other installer settings where applicable.

Arch Linux uses the official text-based installation environment, while Debian uses the stable netinst installer image. Fedora KDE, CachyOS Desktop, and Ubuntu Desktop use their respective graphical installer/live images.


### Clipboard synchronization

`vmforge` does **not** use a SPICE display server. This avoids the incompatibilities between SPICE display output and the accelerated Venus graphics path.

For clipboard synchronization, `vmforge` uses QEMU's built-in `qemu-vdagent` implementation together with a local VNC display. The guest still uses the standard `spice-vdagent` service, because `qemu-vdagent` speaks the same agent protocol. QEMU supports this arrangement with VNC, and TigerVNC provides the host-side clipboard integration.

On CachyOS / Arch Linux, install TigerVNC:

```bash
sudo pacman -S tigervnc
```

Inside a Fedora guest:

```bash
sudo dnf install spice-vdagent
```

Inside an Arch/CachyOS guest:

```bash
sudo pacman -S spice-vdagent
```

Make sure the user-session agent is running. On a desktop session, the package normally starts the agent automatically; when debugging, check:

```bash
systemctl --user status spice-vdagent
```

The VM exposes the agent protocol on:

```text
com.redhat.spice.0
```

The name comes from the spice-vdagent protocol; **no `spicevmc` backend and no SPICE server are used**.

`vmforge start NAME` starts QEMU with Venus plus a localhost-only VNC server and opens `vncviewer` in the background supervisor. The command returns after the VM is ready; it is safe to close the terminal. Diagnostics are written to `~/.local/share/vmforge/NAME/vmforge.log`. To open another viewer for an already running VM:

```bash
vmforge connect NAME
```

Closing the viewer does not stop the VM, and closing the terminal does not stop either the VM or its virtiofs shares. Stop the VM with:

```bash
vmforge stop NAME
```

To inspect startup/runtime errors:

```bash
tail -f ~/.local/share/vmforge/NAME/vmforge.log
```

### Venus graphics and display architecture

The default installed-guest graphics path is:

```text
virtio-vga-gl + hostmem + blob + venus
             |
             v
       egl-headless
             |
             v
      local VNC server
             |
             v
          vncviewer
```

QEMU documents `virtio-gpu-gl,hostmem=...,blob=true,venus=true` as the Venus configuration. It also documents `egl-headless` as a GL offload backend that can be paired with VNC. This keeps the accelerated virtio-gpu device separate from the desktop display client.

Clipboard traffic uses:

```text
vncviewer <-> QEMU VNC clipboard <-> qemu-vdagent
                         |
                         v
              com.redhat.spice.0
                         |
                         v
                  spice-vdagent
                         |
                         v
                 guest clipboard
```

This requires a QEMU build with the `qemu-vdagent` chardev and a VNC client that supports clipboard synchronization. On Arch/CachyOS, the `tigervnc` package provides `vncviewer`.

### CachyOS / Arch host packages

A practical host setup is:

```bash
sudo pacman -Syu qemu-desktop tigervnc
```

For virtiofs shares you also need `virtiofsd`.

`qemu-desktop` pulls in the QEMU desktop components, including the virtio-gpu GL device, VNC-related display infrastructure and EGL headless support. Current Arch package metadata lists these as split packages of QEMU.

### Troubleshooting clipboard

First check the host:

```bash
vmforge doctor
```

Look for:

```text
qemu-vdagent chardev: supported
vncviewer: found
```

Then check the guest:

```bash
ls -l /dev/virtio-ports/com.redhat.spice.0
systemctl --user status spice-vdagent
```

The virtio port should exist while the VM is running.

If the port exists but the clipboard does not synchronize, restart the per-user agent:

```bash
systemctl --user restart spice-vdagent
```

Then copy plain text in both directions. The current QEMU/Arch design uses the `qemu-vdagent` chardev for this VNC clipboard path; `qemu-guest-agent` is a separate channel and is not responsible for clipboard synchronization.

## License

`vmforge` is free software licensed under the GNU Affero General Public License, version 3 or later. See `LICENSE`.
