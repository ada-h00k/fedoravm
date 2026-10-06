//! vmforge - a small Linux VM manager for QEMU/KVM.
//!
//! SPDX-License-Identifier: AGPL-3.0-or-later
//! Copyright © 2026 the vmforge contributors.

use clap::{Args, Parser, Subcommand, ValueEnum};
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

const FEDORA_KDE_PAGE: &str = "https://fedoraproject.org/kde/download/";
const FEDORA_MIRROR_ROOT: &str = "https://dl.fedoraproject.org/pub/fedora/linux/releases";
const CACHYOS_ISO_ROOT: &str = "https://mirror.cachyos.org/ISO/desktop";
const ARCH_DOWNLOAD_PAGE: &str = "https://archlinux.org/download/";
const ARCH_MIRROR_ROOT: &str = "https://geo.mirror.pkgbuild.com/iso/latest";
const DEBIAN_DOWNLOAD_PAGE: &str = "https://www.debian.org/download.en.html";
const DEBIAN_ISO_ROOT: &str = "https://deb.debian.org/debian-cd/current/amd64/iso-cd";
const UBUNTU_DESKTOP_PAGE: &str = "https://ubuntu.com/download/desktop";

#[derive(Debug, Error)]
enum AppError {
    #[error("{0}")]
    Message(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("configuration error: {0}")]
    Config(#[from] serde_json::Error),
}

type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum Distro {
    Fedora,
    Cachyos,
    Arch,
    Debian,
    Ubuntu,
}

impl Default for Distro {
    fn default() -> Self {
        Self::Fedora
    }
}

impl Distro {
    fn display_name(self) -> &'static str {
        match self {
            Self::Fedora => "Fedora KDE Plasma Desktop",
            Self::Cachyos => "CachyOS Desktop",
            Self::Arch => "Arch Linux",
            Self::Debian => "Debian",
            Self::Ubuntu => "Ubuntu Desktop",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Fedora => "fedora",
            Self::Cachyos => "cachyos",
            Self::Arch => "arch",
            Self::Debian => "debian",
            Self::Ubuntu => "ubuntu",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum GraphicsMode {
    /// Accelerated virtio-gpu using virglrenderer with the Venus Vulkan capset.
    Venus,
    /// 2D virtio-gpu only.
    Safe,
}

impl Default for GraphicsMode {
    fn default() -> Self {
        Self::Venus
    }
}

#[derive(Parser, Debug)]
#[command(name = "vmforge", version, about = "Small QEMU/KVM VM manager for Linux distributions")]
struct Cli {
    #[arg(long, env = "VMFORGE_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand, Debug)]
enum CommandKind {
    /// Create a new VM and boot the selected distribution installer.
    Create(CreateArgs),
    /// Start an existing VM from its virtual disk.
    Start(StartArgs),
    /// Start an existing VM and boot its installer once.
    Install(StartArgs),
    /// Send SIGTERM to a running VM.
    Stop(VmRefArgs),
    /// Delete a VM and all of its local state.
    Delete(VmRefArgs),
    /// List known VMs.
    List,
    /// Print host/QEMU/UEFI capabilities relevant to this tool.
    Doctor,
    /// Manage persistent virtiofs shares for an existing VM.
    #[command(subcommand)]
    Share(ShareCommand),
}

#[derive(Subcommand, Debug)]
enum ShareCommand {
    /// Add a host directory to an existing VM.
    Add {
        name: String,
        path: PathBuf,
    },
    /// Remove a share by its zero-based index.
    Remove {
        name: String,
        index: usize,
    },
    /// List configured shares.
    List {
        name: String,
    },
}

#[derive(Args, Debug)]
struct VmRefArgs {
    name: String,
}

#[derive(Args, Debug)]
struct StartArgs {
    name: String,
    /// Override the VM graphics mode for this boot.
    #[arg(long, value_enum)]
    graphics: Option<GraphicsMode>,
}

#[derive(Args, Debug)]
struct CreateArgs {
    /// VM name. Optional with --temp/-temp.
    name: Option<String>,

    /// Delete the VM, disk and firmware vars when QEMU exits.
    #[arg(long, short = 't')]
    temp: bool,

    /// Host directory to expose through virtiofs. Repeatable.
    #[arg(long = "share", value_name = "PATH")]
    shares: Vec<PathBuf>,

    /// Use Fedora KDE Plasma Desktop.
    #[arg(long)]
    fedora: bool,
    /// Use CachyOS Desktop.
    #[arg(long)]
    cachyos: bool,
    /// Use Arch Linux.
    #[arg(long)]
    arch: bool,
    /// Use Debian stable (netinst installer).
    #[arg(long)]
    debian: bool,
    /// Use Ubuntu Desktop LTS.
    #[arg(long)]
    ubuntu: bool,

    /// Alternative generic distro selector: fedora, cachyos, arch, debian, ubuntu.
    #[arg(long, value_enum)]
    distro: Option<Distro>,

    /// Guest RAM, e.g. 8G, 6144M.
    #[arg(long, default_value = "8G")]
    ram: String,

    /// Number of virtual CPUs.
    #[arg(long, default_value_t = 4)]
    cpus: u32,

    /// Virtual disk size, e.g. 64G.
    #[arg(long, default_value = "64G")]
    disk: String,

    /// virtio-gpu host memory window, e.g. 4G.
    #[arg(long = "gpu-memory", default_value = "4G")]
    gpu_memory: String,

    /// Keep accelerated graphics enabled while booting the installer.
    #[arg(long = "installer-3d")]
    installer_3d: bool,

    /// Graphics backend for normal VM boots.
    #[arg(long, value_enum, default_value_t = GraphicsMode::Venus)]
    graphics: GraphicsMode,
}

impl CreateArgs {
    fn selected_distro(&self) -> Result<Distro> {
        let mut selected = Vec::new();
        if self.fedora {
            selected.push(Distro::Fedora);
        }
        if self.cachyos {
            selected.push(Distro::Cachyos);
        }
        if self.arch {
            selected.push(Distro::Arch);
        }
        if self.debian {
            selected.push(Distro::Debian);
        }
        if self.ubuntu {
            selected.push(Distro::Ubuntu);
        }
        if let Some(distro) = self.distro {
            selected.push(distro);
        }

        selected.sort_by_key(|d| d.slug());
        selected.dedup_by_key(|d| d.slug());

        match selected.as_slice() {
            [] => Ok(Distro::Fedora),
            [only] => Ok(*only),
            _ => Err(AppError::Message(
                "choose exactly one distro selector (--fedora, --cachyos, --arch, --debian, --ubuntu, or --distro <name>)".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VmConfig {
    name: String,
    #[serde(default)]
    distro: Distro,
    ram: String,
    cpus: u32,
    disk_size: String,
    gpu_memory: String,
    #[serde(default)]
    installer_3d: bool,
    #[serde(default)]
    graphics: GraphicsMode,
    disk: PathBuf,
    iso: PathBuf,
    uefi_vars: PathBuf,
    shares: Vec<PathBuf>,
    temporary: bool,
    created_at: u64,
}

#[derive(Debug)]
struct OsImage {
    distro: Distro,
    version: String,
    filename: String,
    iso_url: String,
    checksum_url: String,
    sha256: String,
}

fn main() -> Result<()> {
    let args = normalize_args(std::env::args_os());
    let cli = Cli::parse_from(args);
    let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);

    match cli.command {
        CommandKind::Create(args) => create_vm(&data_dir, args),
        CommandKind::Start(args) => start_vm(&data_dir, &args.name, false, args.graphics),
        CommandKind::Install(args) => start_vm(&data_dir, &args.name, true, args.graphics),
        CommandKind::Stop(args) => stop_vm(&data_dir, &args.name),
        CommandKind::Delete(args) => delete_vm(&data_dir, &args.name),
        CommandKind::List => list_vms(&data_dir),
        CommandKind::Doctor => doctor(),
        CommandKind::Share(command) => share_command(&data_dir, command),
    }
}

fn share_command(data_dir: &Path, command: ShareCommand) -> Result<()> {
    match command {
        ShareCommand::Add { name, path } => add_share(data_dir, &name, &path),
        ShareCommand::Remove { name, index } => remove_share(data_dir, &name, index),
        ShareCommand::List { name } => list_shares(data_dir, &name),
    }
}

fn add_share(data_dir: &Path, name: &str, path: &Path) -> Result<()> {
    let mut config = load_config(data_dir, name)?;
    let vm_dir = config_dir(&config)?;

    if is_running(&vm_dir)? {
        return Err(AppError::Message(format!(
            "VM `{name}` is still running; stop it first with `vmforge stop {name}`"
        )));
    }
    if config.shares.len() >= 8 {
        return Err(AppError::Message(
            "at most 8 shares are supported".into(),
        ));
    }

    let canonical = fs::canonicalize(path).map_err(|e| {
        AppError::Message(format!(
            "share path {} is not accessible: {e}",
            path.display()
        ))
    })?;
    if !fs::metadata(&canonical)?.is_dir() {
        return Err(AppError::Message(format!(
            "share is not a directory: {}",
            canonical.display()
        )));
    }

    if config.shares.iter().any(|p| p == &canonical) {
        return Err(AppError::Message(format!(
            "share is already configured: {}",
            canonical.display()
        )));
    }

    config.shares.push(canonical.clone());
    save_config(&vm_dir, &config)?;
    println!(
        "Added {} as share{} to VM `{}`.",
        canonical.display(),
        config.shares.len() - 1,
        name
    );
    Ok(())
}

fn remove_share(data_dir: &Path, name: &str, index: usize) -> Result<()> {
    let mut config = load_config(data_dir, name)?;
    let vm_dir = config_dir(&config)?;

    if is_running(&vm_dir)? {
        return Err(AppError::Message(format!(
            "VM `{name}` is still running; stop it first with `vmforge stop {name}`"
        )));
    }
    if index >= config.shares.len() {
        return Err(AppError::Message(format!(
            "share index {index} does not exist; VM `{name}` has {} share(s)",
            config.shares.len()
        )));
    }

    let removed = config.shares.remove(index);
    save_config(&vm_dir, &config)?;
    println!("Removed share {} from VM `{}`.", removed.display(), name);
    Ok(())
}

fn list_shares(data_dir: &Path, name: &str) -> Result<()> {
    let config = load_config(data_dir, name)?;
    if config.shares.is_empty() {
        println!("VM `{name}` has no shares.");
        return Ok(());
    }

    for (index, path) in config.shares.iter().enumerate() {
        println!("share{index}: {}", path.display());
    }
    Ok(())
}

fn normalize_args<I>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    args.into_iter()
        .map(|arg| match arg.to_string_lossy().as_ref() {
            "-temp" => OsString::from("--temp"),
            "-fedora" => OsString::from("--fedora"),
            "-cachyos" => OsString::from("--cachyos"),
            "-arch" => OsString::from("--arch"),
            "-debian" => OsString::from("--debian"),
            "-ubuntu" => OsString::from("--ubuntu"),
            _ => arg,
        })
        .collect()
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".local/share"))
                .unwrap_or_else(|| PathBuf::from("."))
        })
        .join("vmforge")
}

fn create_vm(data_dir: &Path, args: CreateArgs) -> Result<()> {
    require_linux_x86_64()?;
    require_binary("qemu-system-x86_64")?;
    require_binary("qemu-img")?;
    if !args.shares.is_empty() {
        find_virtiofsd().ok_or_else(|| {
            AppError::Message(
                "virtiofsd was not found. Install virtiofsd and make sure it is in PATH or a standard system path (Arch/CachyOS commonly use /usr/lib/virtiofsd).".into(),
            )
        })?;
    }

    let distro = args.selected_distro()?;
    let name = match args.name {
        Some(name) => validate_name(&name)?,
        None if args.temp => temp_name(),
        None => {
            return Err(AppError::Message(
                "a VM name is required unless --temp/-temp is used".into(),
            ))
        }
    };

    if args.cpus == 0 {
        return Err(AppError::Message("--cpus must be greater than zero".into()));
    }
    if args.shares.len() > 8 {
        return Err(AppError::Message("at most 8 --share arguments are supported".into()));
    }
    for share in &args.shares {
        if !fs::metadata(share).map_err(|e| {
            AppError::Message(format!("share path {} is not accessible: {e}", share.display()))
        })?.is_dir() {
            return Err(AppError::Message(format!("share is not a directory: {}", share.display())));
        }
    }

    let vm_dir = data_dir.join(&name);
    if vm_dir.exists() {
        return Err(AppError::Message(format!(
            "VM `{name}` already exists at {}",
            vm_dir.display()
        )));
    }
    fs::create_dir_all(&vm_dir)?;
    fs::set_permissions(&vm_dir, fs::Permissions::from_mode(0o700))?;

    println!("Resolving the current {} image …", distro.display_name());
    let image = resolve_image(distro)?;
    println!("{} {}", image.distro.display_name(), image.version);

    let iso_cache = data_dir.join("cache").join(distro.slug()).join(&image.filename);
    fs::create_dir_all(iso_cache.parent().unwrap())?;
    download_and_verify(&image, &iso_cache)?;

    let disk = vm_dir.join(format!("{name}.qcow2"));
    run_checked(
        Command::new("qemu-img")
            .args(["create", "-f", "qcow2", "-o", "preallocation=metadata"])
            .arg(&disk)
            .arg(&args.disk),
        "qemu-img could not create the VM disk",
    )?;

    let (ovmf_code, ovmf_vars_template) = find_ovmf()?;
    let uefi_vars = vm_dir.join("OVMF_VARS.fd");
    fs::copy(&ovmf_vars_template, &uefi_vars)?;

    let config = VmConfig {
        name: name.clone(),
        distro,
        ram: args.ram,
        cpus: args.cpus,
        disk_size: args.disk,
        gpu_memory: args.gpu_memory,
        installer_3d: args.installer_3d,
        graphics: args.graphics,
        disk,
        iso: iso_cache,
        uefi_vars,
        shares: args.shares,
        temporary: args.temp,
        created_at: now_secs(),
    };
    save_config(&vm_dir, &config)?;

    println!("Created VM: {}", vm_dir.display());
    println!("Booting the {} installer …", config.distro.display_name());
    let result = run_qemu(&config, &ovmf_code, true, None);

    if config.temporary {
        println!("Temporary VM: removing VM state …");
        let _ = fs::remove_dir_all(&vm_dir);
    }

    result
}

fn resolve_image(distro: Distro) -> Result<OsImage> {
    match distro {
        Distro::Fedora => resolve_fedora_image(),
        Distro::Cachyos => resolve_cachyos_image(),
        Distro::Arch => resolve_arch_image(),
        Distro::Debian => resolve_debian_image(),
        Distro::Ubuntu => resolve_ubuntu_image(),
    }
}

fn resolve_fedora_image() -> Result<OsImage> {
    let client = http_client()?;
    let html = client.get(FEDORA_KDE_PAGE).send()?.error_for_status()?.text()?;
    let re = Regex::new(r"Fedora-KDE-(\d+)-([0-9][0-9A-Za-z._-]*)-x86_64-CHECKSUM")
        .map_err(|e| AppError::Message(e.to_string()))?;
    let captures = re.captures(&html).ok_or_else(|| {
        AppError::Message("Fedora KDE download page does not expose a matching x86_64 checksum file".into())
    })?;
    let version = captures[1].to_string();
    let respin = captures[2].to_string();
    let filename = format!("Fedora-KDE-Desktop-Live-{version}-{respin}.x86_64.iso");
    let checksum_name = format!("Fedora-KDE-{version}-{respin}-x86_64-CHECKSUM");
    let base = format!("{FEDORA_MIRROR_ROOT}/{version}/KDE/x86_64/iso");
    let checksum_url = format!("{base}/{checksum_name}");
    let checksum_text = client.get(&checksum_url).send()?.error_for_status()?.text()?;
    let sha256 = parse_checksum(&checksum_text, &filename)?;
    Ok(OsImage {
        distro: Distro::Fedora,
        version,
        filename,
        iso_url: format!("{base}/{filename}"),
        checksum_url,
        sha256,
    })
}

fn resolve_cachyos_image() -> Result<OsImage> {
    let client = http_client()?;
    let html = client.get(format!("{CACHYOS_ISO_ROOT}/")).send()?.error_for_status()?.text()?;
    let re = Regex::new(r">(\d{6})/\s*<").map_err(|e| AppError::Message(e.to_string()))?;
    let version = re
        .captures_iter(&html)
        .map(|c| c[1].to_string())
        .max()
        .ok_or_else(|| AppError::Message("could not determine the latest CachyOS desktop ISO directory".into()))?;
    let dir_url = format!("{CACHYOS_ISO_ROOT}/{version}/");
    let dir_html = client.get(&dir_url).send()?.error_for_status()?.text()?;
    let iso_re = Regex::new(&format!(r">(cachyos-desktop-linux-{}\.iso)\s*<", regex::escape(&version)))
        .map_err(|e| AppError::Message(e.to_string()))?;
    let filename = iso_re
        .captures(&dir_html)
        .map(|c| c[1].to_string())
        .or_else(|| {
            let fallback = format!("cachyos-desktop-linux-{version}.iso");
            dir_html.contains(&fallback).then_some(fallback)
        })
        .ok_or_else(|| AppError::Message(format!("CachyOS mirror does not expose the expected ISO for {version}")))?;
    let iso_url = format!("{dir_url}{filename}");
    let checksum_url = format!("{iso_url}.sha256");
    let checksum_text = client.get(&checksum_url).send()?.error_for_status()?.text()?;
    let sha256 = parse_checksum(&checksum_text, &filename)?;
    Ok(OsImage {
        distro: Distro::Cachyos,
        version,
        filename,
        iso_url,
        checksum_url,
        sha256,
    })
}

fn resolve_arch_image() -> Result<OsImage> {
    let client = http_client()?;
    let html = client.get(ARCH_DOWNLOAD_PAGE).send()?.error_for_status()?.text()?;
    let re = Regex::new(r"Current Release:\s*([0-9]+\.[0-9]+\.[0-9]+)").map_err(|e| AppError::Message(e.to_string()))?;
    let version = re
        .captures(&html)
        .map(|c| c[1].to_string())
        .ok_or_else(|| AppError::Message("could not determine the current Arch Linux release".into()))?;
    let filename = format!("archlinux-{version}-x86_64.iso");
    let iso_url = format!("{ARCH_MIRROR_ROOT}/{filename}");
    let checksum_url = format!("{ARCH_MIRROR_ROOT}/sha256sums.txt");
    let checksum_text = client.get(&checksum_url).send()?.error_for_status()?.text()?;
    let sha256 = parse_checksum(&checksum_text, &filename)?;
    Ok(OsImage {
        distro: Distro::Arch,
        version,
        filename,
        iso_url,
        checksum_url,
        sha256,
    })
}

fn resolve_debian_image() -> Result<OsImage> {
    let client = http_client()?;
    let html = client.get(DEBIAN_DOWNLOAD_PAGE).send()?.error_for_status()?.text()?;
    let re = Regex::new(r"debian-(\d+\.\d+\.\d+)-amd64-netinst\.iso").map_err(|e| AppError::Message(e.to_string()))?;
    let filename = re
        .captures(&html)
        .map(|c| c.get(0).unwrap().as_str().to_string())
        .ok_or_else(|| AppError::Message("could not determine the current Debian amd64 netinst ISO".into()))?;
    let version = re
        .captures(&filename)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| "stable".into());
    let iso_url = format!("{DEBIAN_ISO_ROOT}/{filename}");
    let checksum_url = format!("{DEBIAN_ISO_ROOT}/SHA256SUMS");
    let checksum_text = client.get(&checksum_url).send()?.error_for_status()?.text()?;
    let sha256 = parse_checksum(&checksum_text, &filename)?;
    Ok(OsImage {
        distro: Distro::Debian,
        version,
        filename,
        iso_url,
        checksum_url,
        sha256,
    })
}

fn resolve_ubuntu_image() -> Result<OsImage> {
    let client = http_client()?;
    let html = client.get(UBUNTU_DESKTOP_PAGE).send()?.error_for_status()?.text()?;
    let re = Regex::new(r"Ubuntu\s+(\d+\.\d+\.\d+)\s+LTS").map_err(|e| AppError::Message(e.to_string()))?;
    let version = re
        .captures(&html)
        .map(|c| c[1].to_string())
        .ok_or_else(|| AppError::Message("could not determine the current Ubuntu Desktop LTS version".into()))?;
    let filename = format!("ubuntu-{version}-desktop-amd64.iso");
    let iso_url = format!("https://releases.ubuntu.com/{version}/{filename}");
    let checksum_url = format!("https://releases.ubuntu.com/{version}/SHA256SUMS");
    let checksum_text = client.get(&checksum_url).send()?.error_for_status()?.text()?;
    let sha256 = parse_checksum(&checksum_text, &filename)?;
    Ok(OsImage {
        distro: Distro::Ubuntu,
        version,
        filename,
        iso_url,
        checksum_url,
        sha256,
    })
}

fn parse_checksum(text: &str, filename: &str) -> Result<String> {
    for line in text.lines() {
        if line.contains(filename) {
            let mut fields = line.split_whitespace();
            if let Some(hash) = fields.next() {
                if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Ok(hash.to_ascii_lowercase());
                }
            }
        }
    }
    Err(AppError::Message(format!(
        "no SHA-256 entry for {filename} found in {filename} checksum data"
    )))
}

fn download_and_verify(image: &OsImage, target: &Path) -> Result<()> {
    let expected = &image.sha256;
    let known_good = target.with_extension("iso.sha256");

    if target.exists() && known_good.exists() {
        let stored = fs::read_to_string(&known_good)?.trim().to_ascii_lowercase();
        if stored == *expected {
            println!("ISO already cached: {}", target.display());
            return Ok(());
        }
    }

    println!("Downloading ISO: {}", image.iso_url);
    println!("Checksum source: {}", image.checksum_url);
    let client = http_client()?;
    let mut response = client.get(&image.iso_url).send()?.error_for_status()?;
    let total = response.content_length();
    let temp = target.with_extension("iso.part");
    let mut out = File::create(&temp)?;
    let mut hasher = Sha256::new();
    let mut downloaded = 0u64;
    let mut buf = [0u8; 1024 * 1024];
    let mut last_bucket = 0u64;

    loop {
        let n = response.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        downloaded += n as u64;
        if let Some(total) = total {
            let bucket = (downloaded.saturating_mul(100) / total).min(100);
            if bucket >= last_bucket + 5 {
                print!("\rDownload: {bucket:>3}%");
                io::stdout().flush().ok();
                last_bucket = bucket;
            }
        }
    }
    println!();
    out.sync_all()?;
    let actual = format!("{:x}", hasher.finalize());
    if actual != *expected {
        let _ = fs::remove_file(&temp);
        return Err(AppError::Message(format!(
            "SHA-256 mismatch for {}: expected {}, got {}",
            image.filename, expected, actual
        )));
    }
    fs::rename(temp, target)?;
    fs::write(&known_good, format!("{expected}\n"))?;
    println!("ISO verified: SHA-256 {actual}");
    Ok(())
}

fn start_vm(data_dir: &Path, name: &str, installer: bool, graphics_override: Option<GraphicsMode>) -> Result<()> {
    let config = load_config(data_dir, name)?;
    let vm_dir = config_dir(&config)?;
    if is_running(&vm_dir)? {
        return Err(AppError::Message(format!("VM `{name}` is already running")));
    }
    let (ovmf_code, _) = find_ovmf()?;
    if installer {
        println!("Booting {} installer …", config.distro.display_name());
    }
    run_qemu(&config, &ovmf_code, installer, graphics_override)
}

fn run_qemu(
    config: &VmConfig,
    ovmf_code: &Path,
    installer: bool,
    graphics_override: Option<GraphicsMode>,
) -> Result<()> {
    ensure_kvm_support()?;

    let selected_graphics = graphics_override.unwrap_or(config.graphics);
    let accelerated_installer = installer && config.installer_3d;
    let use_venus = match selected_graphics {
        GraphicsMode::Venus => !installer || accelerated_installer,
        GraphicsMode::Safe => false,
    };
    if use_venus {
        ensure_venus_support()?;
    }

    let vm_dir = config_dir(config)?;
    fs::create_dir_all(&vm_dir)?;
    let pid_file = vm_dir.join("qemu.pid");
    if pid_file.exists() {
        let _ = fs::remove_file(&pid_file);
    }

    let mut virtiofs_children = Vec::<Child>::new();
    let mut args = vec![
        "-name".into(), config.name.clone().into(),
        "-machine".into(), "q35".into(),
        "-accel".into(), "kvm".into(),
        "-cpu".into(), "host".into(),
        "-smp".into(), config.cpus.to_string().into(),
        "-pidfile".into(), pid_file.as_os_str().into(),
        "-vga".into(), "none".into(),
        "-display".into(), if use_venus { display_backend_gl() } else { "gtk,gl=off".into() },
        "-drive".into(), format!("if=pflash,format=raw,readonly=on,file={}", path_arg(ovmf_code)).into(),
        "-drive".into(), format!("if=pflash,format=raw,file={}", path_arg(&config.uefi_vars)).into(),
        "-drive".into(), format!("if=none,id=disk0,format=qcow2,file={},discard=unmap,cache=writeback", path_arg(&config.disk)).into(),
        "-device".into(), "virtio-blk-pci,drive=disk0".into(),
        "-netdev".into(), "user,id=net0".into(),
        "-device".into(), "virtio-net-pci,netdev=net0".into(),
        "-device".into(), "ich9-intel-hda".into(),
        "-device".into(), "hda-duplex".into(),
        "-boot".into(), if installer { "once=d,menu=on".into() } else { "strict=on".into() },
    ];

    // vhost-user-fs requires a shared memory backend. Keep -m in sync with
    // the memory-backend-memfd size; otherwise QEMU rejects the NUMA config.
    if config.shares.is_empty() {
        args.extend(["-m".into(), config.ram.clone().into()]);
    } else {
        args.extend([
            "-m".into(), config.ram.clone().into(),
            "-object".into(), format!("memory-backend-memfd,id=mem,size={},share=on", config.ram).into(),
            "-numa".into(), "node,memdev=mem".into(),
        ]);
    }

    if use_venus {
        let gpu_device = if qemu_has_device("virtio-vga-gl") {
            format!("virtio-vga-gl,hostmem={},blob=true,venus=true", config.gpu_memory)
        } else {
            format!("virtio-gpu-gl,hostmem={},blob=true,venus=true", config.gpu_memory)
        };
        args.extend(["-device".into(), gpu_device.into()]);
    } else {
        args.extend(["-device".into(), "virtio-gpu".into()]);
    }

    if installer {
        args.extend(["-cdrom".into(), config.iso.as_os_str().into()]);
    }

    for (idx, share) in config.shares.iter().enumerate() {
        let socket = vm_dir.join(format!("virtiofs-{idx}.sock"));
        let _ = fs::remove_file(&socket);
        let virtiofsd = find_virtiofsd().ok_or_else(|| {
            AppError::Message("virtiofsd was not found".into())
        })?;
        let mut child = match spawn_virtiofsd(&virtiofsd, &socket, share) {
            Ok(child) => child,
            Err(e) => {
                for existing in &mut virtiofs_children {
                    let _ = existing.kill();
                    let _ = existing.wait();
                }
                return Err(e);
            }
        };
        if let Err(e) = wait_for_socket(&socket, Duration::from_secs(5)) {
            let _ = child.kill();
            let _ = child.wait();
            for existing in &mut virtiofs_children {
                let _ = existing.kill();
                let _ = existing.wait();
            }
            return Err(e);
        }
        virtiofs_children.push(child);
        args.extend([
            "-chardev".into(), format!("socket,id=char{idx},path={}", path_arg(&socket)).into(),
            "-device".into(), format!("vhost-user-fs-pci,chardev=char{idx},tag=share{idx},queue-size=1024").into(),
        ]);
    }

    println!("QEMU: qemu-system-x86_64 {}", render_command(&args));
    let status = Command::new("qemu-system-x86_64")
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    for mut child in virtiofs_children {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = fs::remove_file(&pid_file);
    for idx in 0..config.shares.len() {
        let _ = fs::remove_file(vm_dir.join(format!("virtiofs-{idx}.sock")));
    }

    let status = status.map_err(|e| AppError::Message(format!("QEMU could not be started: {e}")))?;
    if !status.success() {
        return Err(AppError::Message(format!("QEMU exited with status {status}")));
    }
    Ok(())
}

fn display_backend_gl() -> OsString {
    if std::env::var_os("XDG_SESSION_TYPE").as_deref() == Some(std::ffi::OsStr::new("wayland"))
        && qemu_has_display("sdl")
    {
        return OsString::from("sdl,gl=on");
    }
    OsString::from("gtk,gl=on")
}

fn qemu_has_display(name: &str) -> bool {
    let Ok(output) = Command::new("qemu-system-x86_64").args(["-display", "help"]).output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().any(|line| line.trim_start().starts_with(name))
}

fn qemu_has_device(name: &str) -> bool {
    let Ok(output) = Command::new("qemu-system-x86_64").args(["-device", name, "help"]).output() else {
        return false;
    };
    output.status.success()
}

fn spawn_virtiofsd(binary: &Path, socket: &Path, share: &Path) -> Result<Child> {
    let root = unsafe { libc::geteuid() } == 0;
    let mut cmd = if root {
        Command::new(binary)
    } else {
        let mut c = Command::new("unshare");
        c.args(["-r", "--map-auto", "--"]);
        c.arg(binary);
        c
    };

    cmd.args([
        OsString::from("--socket-path"), socket.as_os_str().into(),
        OsString::from("--shared-dir"), share.as_os_str().into(),
        OsString::from("--sandbox"), OsString::from(if root { "namespace" } else { "chroot" }),
        OsString::from("--cache"), OsString::from("auto"),
    ]);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AppError::Message(format!("virtiofsd could not be started: {e}")))?;

    thread::sleep(Duration::from_millis(100));
    if let Some(status) = child.try_wait()? {
        let mut msg = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut msg);
        }
        return Err(AppError::Message(format!(
            "virtiofsd exited immediately ({status}): {}",
            msg.trim()
        )));
    }
    Ok(child)
}

fn wait_for_socket(path: &Path, timeout: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(AppError::Message(format!(
        "virtiofsd did not create its socket: {}",
        path.display()
    )))
}

fn stop_vm(data_dir: &Path, name: &str) -> Result<()> {
    let config = load_config(data_dir, name)?;
    let vm_dir = config_dir(&config)?;
    let pid_file = vm_dir.join("qemu.pid");
    if !pid_file.exists() {
        println!("VM `{name}` is not running.");
        return Ok(());
    }
    let pid: i32 = fs::read_to_string(&pid_file)?.trim().parse().map_err(|_| {
        AppError::Message("invalid QEMU PID file".into())
    })?;
    if !is_pid_alive(pid) {
        let _ = fs::remove_file(&pid_file);
        println!("VM `{name}` is no longer running; removed stale PID file.");
        return Ok(());
    }
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    if rc != 0 {
        return Err(AppError::Message(format!(
            "SIGTERM to QEMU PID {pid} failed: {}",
            io::Error::last_os_error()
        )));
    }
    println!("Sent SIGTERM to VM `{name}` (PID {pid}).");
    Ok(())
}

fn delete_vm(data_dir: &Path, name: &str) -> Result<()> {
    let config = load_config(data_dir, name)?;
    let vm_dir = config_dir(&config)?;
    if is_running(&vm_dir)? {
        return Err(AppError::Message(format!(
            "VM `{name}` is still running; stop it first with `vmforge stop {name}`"
        )));
    }
    fs::remove_dir_all(&vm_dir)?;
    println!("Deleted VM `{name}`.");
    Ok(())
}

fn list_vms(data_dir: &Path) -> Result<()> {
    let mut dirs = vec![data_dir.to_path_buf()];
    if let Some(parent) = data_dir.parent() {
        if data_dir.file_name() == Some(std::ffi::OsStr::new("vmforge")) {
            let legacy = parent.join("fedoravm");
            if legacy.is_dir() {
                dirs.push(legacy);
            }
        }
    }

    let mut found = false;
    for root in dirs {
        if !root.exists() {
            continue;
        }
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let cfg_path = entry.path().join("vm.json");
            if !cfg_path.exists() {
                continue;
            }
            found = true;
            let cfg = VmConfig::load(&cfg_path)?;
            let running = is_running(&entry.path())?;
            println!(
                "{:<20} {:<10} Distro={:<8} RAM={} CPU={} Graphics={:?} Shares={}",
                cfg.name,
                if running { "running" } else { "stopped" },
                cfg.distro.slug(),
                cfg.ram,
                cfg.cpus,
                cfg.graphics,
                cfg.shares.len()
            );
        }
    }

    if !found {
        println!("No VMs.");
    }
    Ok(())
}

fn doctor() -> Result<()> {
    println!("vmforge doctor");
    println!("  Linux x86_64: {}", cfg!(target_os = "linux") && cfg!(target_arch = "x86_64"));
    report_binary("qemu-system-x86_64");
    report_binary("qemu-img");
    println!(
        "  virtiofsd: {}",
        find_virtiofsd().map(|p| p.display().to_string()).unwrap_or_else(|| "NOT FOUND".into())
    );
    println!("  /dev/kvm: {}", if Path::new("/dev/kvm").exists() { "present" } else { "missing" });
    println!("  QEMU SDL display: {}", if qemu_has_display("sdl") { "supported" } else { "not available" });
    println!("  virtio-vga-gl: {}", if qemu_has_device("virtio-vga-gl") { "supported" } else { "not available" });
    match find_ovmf() {
        Ok((code, vars)) => println!("  OVMF: {} / {}", code.display(), vars.display()),
        Err(e) => println!("  OVMF: MISSING ({e})"),
    }
    match qemu_has_venus() {
        Ok(true) => println!("  QEMU virtio-gpu Venus: supported"),
        Ok(false) => println!("  QEMU virtio-gpu Venus: NOT supported"),
        Err(e) => println!("  QEMU virtio-gpu Venus: unknown ({e})"),
    }
    Ok(())
}

impl VmConfig {
    fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }
}

fn save_config(vm_dir: &Path, config: &VmConfig) -> Result<()> {
    fs::write(vm_dir.join("vm.json"), serde_json::to_vec_pretty(config)?)?;
    Ok(())
}

fn load_config(data_dir: &Path, name: &str) -> Result<VmConfig> {
    validate_name(name)?;
    let direct = data_dir.join(name).join("vm.json");
    if direct.exists() {
        return VmConfig::load(&direct);
    }

    // Smooth transition from the old fedoravm name. Existing configs without a
    // distro field default to Fedora for backwards compatibility.
    if data_dir.file_name() == Some(std::ffi::OsStr::new("vmforge")) {
        if let Some(parent) = data_dir.parent() {
            let legacy = parent.join("fedoravm").join(name).join("vm.json");
            if legacy.exists() {
                return VmConfig::load(&legacy);
            }
        }
    }

    Err(AppError::Message(format!("VM `{name}` not found")))
}

fn config_dir(config: &VmConfig) -> Result<PathBuf> {
    config.disk.parent().map(PathBuf::from).ok_or_else(|| {
        AppError::Message("VM configuration has no valid directory".into())
    })
}

fn is_running(vm_dir: &Path) -> Result<bool> {
    let pid_file = vm_dir.join("qemu.pid");
    if !pid_file.exists() {
        return Ok(false);
    }
    let pid: i32 = fs::read_to_string(pid_file)?.trim().parse().map_err(|_| {
        AppError::Message("invalid QEMU PID file".into())
    })?;
    Ok(is_pid_alive(pid))
}

fn is_pid_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn validate_name(name: &str) -> Result<String> {
    if name.is_empty() || name.len() > 64 {
        return Err(AppError::Message("VM name must be 1..64 characters long".into()));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(AppError::Message(
            "VM name may only contain A-Z, a-z, 0-9, - and _".into(),
        ));
    }
    Ok(name.to_string())
}

fn temp_name() -> String {
    format!("temp-{}-{}", std::process::id(), now_secs())
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn http_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent("vmforge/0.2")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(6 * 60 * 60))
        .build()?)
}

fn require_binary(name: &str) -> Result<()> {
    if find_binary(name).is_none() {
        return Err(AppError::Message(format!("`{name}` was not found")));
    }
    Ok(())
}

fn report_binary(name: &str) {
    println!(
        "  {name}: {}",
        find_binary(name).map(|p| p.display().to_string()).unwrap_or_else(|| "NOT FOUND".into())
    );
}

fn find_binary(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn find_virtiofsd() -> Option<PathBuf> {
    [
        "virtiofsd",
        "/usr/bin/virtiofsd",
        "/usr/lib/virtiofsd",
        "/usr/libexec/virtiofsd",
        "/usr/lib/qemu/vhost-user/virtiofsd",
        "/usr/lib/qemu/virtiofsd",
    ]
    .into_iter()
    .find_map(find_binary)
}

fn find_ovmf() -> Result<(PathBuf, PathBuf)> {
    let directories = [
        "/usr/share/edk2/x64",
        "/usr/share/edk2-ovmf/x64",
        "/usr/share/edk2/ovmf",
        "/usr/share/OVMF",
    ];
    let filename_pairs = [
        ("OVMF_CODE.4m.fd", "OVMF_VARS.4m.fd"),
        ("OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd"),
        ("OVMF_CODE.fd", "OVMF_VARS.fd"),
        ("OVMF_CODE_4M.secboot.fd", "OVMF_VARS_4M.fd"),
        ("OVMF_CODE.secboot.4m.fd", "OVMF_VARS.4m.fd"),
    ];

    for directory in directories {
        for (code_name, vars_name) in filename_pairs {
            let code = Path::new(directory).join(code_name);
            let vars = Path::new(directory).join(vars_name);
            if code.is_file() && vars.is_file() {
                return Ok((code, vars));
            }
        }
    }

    for directory in directories {
        let dir = Path::new(directory);
        let Ok(entries) = fs::read_dir(dir) else { continue };
        let mut code_candidates = Vec::new();
        let mut vars_candidates = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if !path.is_file() { continue }
            if name.starts_with("OVMF_CODE") && name.ends_with(".fd") && !name.contains("secboot") {
                code_candidates.push(path);
            } else if name.starts_with("OVMF_VARS") && name.ends_with(".fd") {
                vars_candidates.push(path);
            }
        }
        code_candidates.sort();
        vars_candidates.sort();
        for code in code_candidates {
            let Some(code_name) = code.file_name().and_then(|n| n.to_str()) else { continue };
            let suffix = code_name.strip_prefix("OVMF_CODE").unwrap_or("");
            let expected_vars = format!("OVMF_VARS{suffix}");
            if let Some(vars) = vars_candidates.iter().find(|p| {
                p.file_name().and_then(|n| n.to_str()) == Some(expected_vars.as_str())
            }) {
                return Ok((code, vars.clone()));
            }
        }
    }

    Err(AppError::Message(
        "OVMF/EDK2 UEFI firmware was not found. Install an edk2-ovmf package and run `vmforge doctor` to inspect detected paths.".into(),
    ))
}

fn ensure_kvm_support() -> Result<()> {
    if !Path::new("/dev/kvm").exists() {
        return Err(AppError::Message(
            "/dev/kvm is missing. Enable KVM/hardware virtualization on the host.".into(),
        ));
    }
    Ok(())
}

fn ensure_venus_support() -> Result<()> {
    if qemu_has_venus()? {
        Ok(())
    } else {
        Err(AppError::Message(
            "the installed QEMU does not expose virtio-gpu Venus support; update QEMU/virglrenderer or use `--graphics safe`".into(),
        ))
    }
}

fn qemu_has_venus() -> Result<bool> {
    for device in ["virtio-gpu-gl", "virtio-vga-gl"] {
        let output = Command::new("qemu-system-x86_64")
            .args(["-device", device, "help"])
            .output()?;
        if output.status.success() && String::from_utf8_lossy(&output.stdout).contains("venus") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn require_linux_x86_64() -> Result<()> {
    if !cfg!(target_os = "linux") || !cfg!(target_arch = "x86_64") {
        return Err(AppError::Message(
            "vmforge currently supports Linux/x86_64 hosts".into(),
        ));
    }
    Ok(())
}

fn run_checked(command: &mut Command, what: &str) -> Result<()> {
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::Message(format!("{what}: status {status}")))
    }
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn render_command(args: &[OsString]) -> String {
    args.iter().map(|s| shell_quote(&s.to_string_lossy())).collect::<Vec<_>>().join(" ")
}

fn shell_quote(s: &str) -> String {
    if s.bytes().all(|b| matches!(b,
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' |
        b'_' | b'-' | b'.' | b'/' | b':' | b',' | b'=' | b'+'
    )) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}
