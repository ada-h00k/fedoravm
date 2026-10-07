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
vmforge start NAME [--graphics venus|safe]
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


### SPICE on CachyOS / Arch Linux

`vmforge` uses SPICE for the `spice-vdagent` clipboard channel. Arch-family QEMU packages are split, so a minimal `qemu-base` installation does not necessarily include the SPICE chardev backend. On CachyOS, install `qemu-chardev-spice` (or the complete `qemu-full` package) before starting a VM with SPICE clipboard support.

```bash
sudo pacman -S qemu-chardev-spice
# or
sudo pacman -S qemu-full
```

`vmforge doctor` should then report the SPICE chardev as available.

### SPICE agent and clipboard

`vmforge` exposes a dedicated SPICE agent channel (`com.redhat.spice.0`) in every VM in addition to the QEMU Guest Agent channel. This is the channel used by Linux `spice-vdagentd`/`spice-vdagent` for clipboard integration and dynamic display features. The host-side SPICE server listens on a per-VM Unix socket and is not exposed on the network.

After installing `spice-vdagent` in the guest, start the VM and connect with:

```bash
vmforge connect <name>
```

The command uses `remote-viewer` from the `virt-viewer` package. Clipboard sharing then uses the SPICE client and the guest's `spice-vdagent` stack. The guest side needs a virtio-serial controller plus a `spicevmc` channel named `com.redhat.spice.0`, which is what vmforge adds automatically.

On Arch/CachyOS:

```bash
sudo pacman -S virt-viewer spice-vdagent
```

For example:

```bash
vmforge start compile
vmforge connect compile
```

Verify the guest side with:

```bash
ls -l /dev/virtio-ports/com.redhat.spice.0
systemctl status spice-vdagentd.socket
systemctl --user status spice-vdagent
```

The system daemon can legitimately appear as inactive when there is no active session; the socket and the per-user `spice-vdagent` session service are the more useful checks.

The existing local QEMU window can still be used, but the SPICE client window is the one that provides the SPICE clipboard channel. QEMU's local GTK/SDL clipboard mechanisms are separate from `spice-vdagentd`. On Plasma Wayland, also verify that the user-session `spice-vdagent` service is running; `spice-vdagentd` is only the system-side daemon.

## License

`vmforge` is licensed under the **GNU Affero General Public License v3.0 or later**. See [LICENSE](LICENSE).


### QEMU Venus detection

`vmforge` checks the actual QEMU device properties using `-device <device>,help` and accepts Venus when the `venus` property is exposed by either `virtio-gpu-gl` or `virtio-vga-gl`.


## SPICE on Arch/CachyOS

On current Arch-based distributions, QEMU's SPICE chardev backend is shipped as a loadable module (`qemu-chardev-spice`, typically `/usr/lib/qemu/chardev-spice.so`). `vmforge` probes the actual backend instead of relying on `qemu-system-x86_64 -chardev help`, which can report a false negative when the module has not yet been loaded.

To verify the installed module:

```bash
pacman -Qo /usr/lib/qemu/chardev-spice.so
```

Keep the QEMU split packages on the same version, for example `qemu-base`, `qemu-common`, and `qemu-chardev-spice`.


### Troubleshooting SPICE on CachyOS / Arch

`vmforge` does not use `qemu-system-x86_64 -chardev help` to decide whether SPICE is installed. Current Arch packages split optional QEMU drivers into loadable modules; `qemu-chardev-spice` provides the SPICE chardev driver as `/usr/lib/qemu/chardev-spice.so`. `vmforge` probes the real `spicevmc` backend instead.

Check the module with:

```bash
pacman -Qo /usr/lib/qemu/chardev-spice.so
```

You can also run:

```bash
vmforge doctor
```

and look for `QEMU SPICE chardev: supported`.


### SPICE on modern QEMU

`vmforge` uses QEMU's current Unix-socket syntax for SPICE:

```text
-spice unix=on,addr=/path/to/spice.sock,disable-ticketing=on
```

Older examples using `-spice unix=/path/to/socket` are not valid with current QEMU because `unix` is a boolean option; the socket path is supplied through `addr`. See the QEMU invocation documentation.

### SPICE and Clipboard on Arch/CachyOS

When accelerated graphics (Venus) is enabled, `vmforge` must not combine a GL-enabled SDL/GTK display with a SPICE display server. QEMU rejects that combination. Instead, `vmforge` prefers QEMU's `spice-app,gl=on` display when the optional `qemu-ui-spice-app` module is installed. Otherwise it uses a SPICE Unix socket with `gl=on` and `vmforge connect <name>` / `remote-viewer`. QEMU documents both the `spice-app` display and SPICE OpenGL configuration; the SPICE manual recommends native SPICE GL where possible.

On Arch/CachyOS, the embedded viewer is provided by `qemu-ui-spice-app` (and depends on the matching QEMU/SPICE split packages). If it is not installed, install it with:

```bash
sudo pacman -S qemu-ui-spice-app
```

The guest agent channel remains `com.redhat.spice.0`, so `spice-vdagent` can provide clipboard and dynamic-resolution support.


## Graphics and clipboard

The normal VM configuration intentionally uses one display path only:

```text
virtio-vga-gl + Venus -> QEMU SPICE (GL) -> remote-viewer
                         \-> spice-vdagent -> host/guest clipboard
```

QEMU is started with:

```text
-display egl-headless
-spice gl=on,unix=on,addr=/path/to/spice.sock,disable-ticketing=on,disable-copy-paste=off
-device virtio-vga-gl,hostmem=4G,blob=true,venus=true
```

`egl-headless` provides the host OpenGL context while SPICE transports the display to `remote-viewer`. This avoids mixing SDL/GTK OpenGL with SPICE. QEMU's current documentation explicitly describes `egl-headless` as the OpenGL offload backend to pair with SPICE and documents the `venus=true` virtio-gpu configuration.

Clipboard synchronization requires the guest-side `spice-vdagent`/`spice-vdagentd` package and the QEMU `com.redhat.spice.0` virtio-serial channel. `vmforge` adds that channel automatically.

`vmforge start NAME` launches `remote-viewer` automatically. `vmforge connect NAME` can be used to open another viewer for an already running VM. Closing the viewer leaves the VM running; use `vmforge stop NAME` to stop it.

For safe graphics troubleshooting, `--graphics safe` disables Venus and uses plain virtio-gpu.


## Testing Venus and clipboard

Inside the Linux guest, verify that the virtio GPU is present:

```bash
lspci -nnk | grep -A4 -Ei 'VGA|3D|Display'
```

Verify the SPICE agent port:

```bash
ls -l /dev/virtio-ports/com.redhat.spice.0
```

Make sure the guest agent is installed (`spice-vdagent` on Fedora/Arch) and running in the desktop session. Then copy text in both directions between the host and guest.

For Vulkan/Venus, install `vulkan-tools` and run:

```bash
vulkaninfo --summary
```

The virtio/Venus device should be visible.
