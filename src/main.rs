//! fedoravm - a small Fedora KDE Plasma VM manager for QEMU/KVM.
//!
//! SPDX-License-Identifier: AGPL-3.0-or-later
//! Copyright © 2026 the fedoravm contributors.

use clap::{Args, Parser, Subcommand};
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

const FEDORA_KDE_PAGE: &str = "https://fedoraproject.org/kde/download/";
const FEDORA_MIRROR_ROOT: &str = "https://download.fedoraproject.org/pub/fedora/linux/releases";

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

#[derive(Parser, Debug)]
#[command(name = "fedoravm", version, about = "Fedora KDE Plasma VMs with QEMU/KVM + virtio-gpu Venus")]
struct Cli {
    #[arg(long, env = "FEDORAVM_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand, Debug)]
enum CommandKind {
    /// Create a new VM and boot the Fedora KDE installer once.
    Create(CreateArgs),
    /// Start an existing VM from its virtual disk.
    Start(VmRefArgs),
    /// Start an existing VM and boot the Fedora installer once.
    Install(VmRefArgs),
    /// Send SIGTERM to a running VM.
    Stop(VmRefArgs),
    /// Delete a VM and all of its local state.
    Delete(VmRefArgs),
    /// List known VMs.
    List,
    /// Print host/QEMU/UEFI capabilities relevant to this tool.
    Doctor,
}

#[derive(Args, Debug)]
struct VmRefArgs {
    name: String,
}

#[derive(Args, Debug)]
struct CreateArgs {
    /// VM name. Optional with --temp.
    name: Option<String>,

    /// Delete the VM, disk and firmware vars when QEMU exits.
    #[arg(long, short = 't')]
    temp: bool,

    /// Host directory to expose through virtiofs. Repeatable.
    #[arg(long = "share", value_name = "PATH")]
    shares: Vec<PathBuf>,

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VmConfig {
    name: String,
    ram: String,
    cpus: u32,
    disk_size: String,
    gpu_memory: String,
    disk: PathBuf,
    iso: PathBuf,
    uefi_vars: PathBuf,
    shares: Vec<PathBuf>,
    temporary: bool,
    created_at: u64,
}

#[derive(Debug)]
struct FedoraImage {
    version: u32,
    respin: String,
    iso_name: String,
    iso_url: String,
    checksum_url: String,
    sha256: String,
}

fn main() -> Result<()> {
    // Clap intentionally does not accept `-temp` as a short option. Normalize the
    // requested shorthand before parsing so both `create --temp` and `create -temp` work.
    let args = normalize_args(std::env::args_os());
    let cli = Cli::parse_from(args);
    let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);

    match cli.command {
        CommandKind::Create(args) => create_vm(&data_dir, args),
        CommandKind::Start(args) => start_vm(&data_dir, &args.name, false),
        CommandKind::Install(args) => start_vm(&data_dir, &args.name, true),
        CommandKind::Stop(args) => stop_vm(&data_dir, &args.name),
        CommandKind::Delete(args) => delete_vm(&data_dir, &args.name),
        CommandKind::List => list_vms(&data_dir),
        CommandKind::Doctor => doctor(),
    }
}

fn normalize_args<I>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    args.into_iter()
        .map(|arg| if arg == OsString::from("-temp") { OsString::from("--temp") } else { arg })
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
        .join("fedoravm")
}

fn create_vm(data_dir: &Path, args: CreateArgs) -> Result<()> {
    require_linux_x86_64()?;
    require_binary("qemu-system-x86_64")?;
    require_binary("qemu-img")?;
    find_virtiofsd().ok_or_else(|| AppError::Message(
        "virtiofsd not found. On Fedora install the `virtiofsd` package.".into(),
    ))?;

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
        let meta = fs::metadata(share).map_err(|e| {
            AppError::Message(format!("share path {} is not accessible: {e}", share.display()))
        })?;
        if !meta.is_dir() {
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

    println!("Ermittle aktuelle stabile Fedora KDE Version …");
    let image = fetch_fedora_image()?;
    println!("Fedora KDE {} ({})", image.version, image.respin);

    let iso_cache = data_dir.join("cache").join(&image.iso_name);
    fs::create_dir_all(iso_cache.parent().unwrap())?;
    download_and_verify(&image, &iso_cache)?;

    let disk = vm_dir.join(format!("{name}.qcow2"));
    run_checked(
        Command::new("qemu-img")
            .args(["create", "-f", "qcow2", "-o", "preallocation=metadata"])
            .arg(&disk)
            .arg(&args.disk),
        "qemu-img konnte das VM-Laufwerk nicht anlegen",
    )?;

    let (ovmf_code, ovmf_vars_template) = find_ovmf()?;
    let uefi_vars = vm_dir.join("OVMF_VARS.fd");
    fs::copy(&ovmf_vars_template, &uefi_vars)?;

    let config = VmConfig {
        name: name.clone(),
        ram: args.ram,
        cpus: args.cpus,
        disk_size: args.disk,
        gpu_memory: args.gpu_memory,
        disk,
        iso: iso_cache,
        uefi_vars,
        shares: args.shares,
        temporary: args.temp,
        created_at: now_secs(),
    };
    save_config(&vm_dir, &config)?;

    println!("VM angelegt: {}", vm_dir.display());
    println!("Starte jetzt den Fedora-Installer …");
    let result = run_qemu(&config, &ovmf_code, true);

    if config.temporary {
        println!("Temporary-VM: räume VM-Verzeichnis auf …");
        let _ = fs::remove_dir_all(&vm_dir);
    }

    result
}

fn fetch_fedora_image() -> Result<FedoraImage> {
    let client = http_client()?;
    let html = client.get(FEDORA_KDE_PAGE).send()?.error_for_status()?.text()?;

    let re = Regex::new(r"Fedora-KDE-(\d+)-([0-9][0-9A-Za-z._-]*)-x86_64-CHECKSUM")
        .map_err(|e| AppError::Message(e.to_string()))?;
    let captures = re.captures(&html).ok_or_else(|| {
        AppError::Message("die Fedora-KDE Download-Seite enthält kein passendes x86_64-Release".into())
    })?;

    let version: u32 = captures[1]
        .parse()
        .map_err(|_| AppError::Message("ungültige Fedora Release-Nummer".into()))?;
    let respin = captures[2].to_string();
    let iso_name = format!("Fedora-KDE-Desktop-Live-{version}-{respin}.x86_64.iso");
    let checksum_name = format!("Fedora-KDE-{version}-{respin}-x86_64-CHECKSUM");
    let base = format!("{FEDORA_MIRROR_ROOT}/{version}/KDE/x86_64/iso");
    let iso_url = format!("{base}/{iso_name}");
    let checksum_url = format!("{base}/{checksum_name}");

    let checksum_text = client
        .get(&checksum_url)
        .send()?
        .error_for_status()?
        .text()?;
    let sha256 = parse_checksum(&checksum_text, &iso_name)?;

    Ok(FedoraImage {
        version,
        respin,
        iso_name,
        iso_url,
        checksum_url,
        sha256,
    })
}

fn parse_checksum(text: &str, filename: &str) -> Result<String> {
    let re = Regex::new(r"(?i)([0-9a-f]{64}).*")
        .map_err(|e| AppError::Message(e.to_string()))?;
    for line in text.lines() {
        if line.contains(filename) {
            if let Some(caps) = re.captures(line) {
                return Ok(caps[1].to_ascii_lowercase());
            }
        }
    }
    Err(AppError::Message(format!(
        "kein SHA256-Eintrag für {filename} in der Fedora-Checksum-Datei gefunden ({})",
        filename
    )))
}

fn download_and_verify(image: &FedoraImage, target: &Path) -> Result<()> {
    let expected = &image.sha256;
    let known_good = target.with_extension("iso.sha256");

    if target.exists() && known_good.exists() {
        let stored = fs::read_to_string(&known_good)?.trim().to_ascii_lowercase();
        if stored == *expected {
            println!("ISO bereits im Cache: {}", target.display());
            return Ok(());
        }
    }

    println!("Lade ISO herunter: {}", image.iso_url);
    println!("Checksum-Datei: {}", image.checksum_url);
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
            "SHA256 mismatch für {}: erwartet {}, erhalten {}",
            image.iso_name, expected, actual
        )));
    }
    fs::rename(temp, target)?;
    fs::write(&known_good, format!("{expected}\n"))?;
    println!("ISO verifiziert: SHA256 {actual}");
    Ok(())
}

fn start_vm(data_dir: &Path, name: &str, installer: bool) -> Result<()> {
    let config = load_config(data_dir, name)?;
    let vm_dir = config_dir(&config)?;
    if is_running(&vm_dir)? {
        return Err(AppError::Message(format!("VM `{name}` läuft bereits")));
    }
    let (ovmf_code, _) = find_ovmf()?;

    if installer {
        println!("Boot once from Fedora KDE installer …");
    }
    run_qemu(&config, &ovmf_code, installer)
}

fn run_qemu(config: &VmConfig, ovmf_code: &Path, installer: bool) -> Result<()> {
    ensure_kvm_support()?;
    ensure_venus_support()?;

    let vm_dir = config_dir(config)?;
    fs::create_dir_all(&vm_dir)?;
    let pid_file = vm_dir.join("qemu.pid");
    if pid_file.exists() {
        let _ = fs::remove_file(&pid_file);
    }

    let mut virtiofs_children = Vec::<Child>::new();
    let mut args = Vec::<OsString>::new();

    args.extend([
        "-name".into(), config.name.clone().into(),
        "-machine".into(), "q35".into(),
        "-accel".into(), "kvm".into(),
        "-cpu".into(), "host".into(),
        "-smp".into(), config.cpus.to_string().into(),
        "-pidfile".into(), pid_file.as_os_str().into(),
        "-vga".into(), "none".into(),
        "-display".into(), "gtk,gl=on".into(),
        "-device".into(), format!("virtio-gpu-gl,hostmem={},blob=true,venus=true", config.gpu_memory).into(),
        "-drive".into(), format!("if=pflash,format=raw,readonly=on,file={}", path_arg(ovmf_code)).into(),
        "-drive".into(), format!("if=pflash,format=raw,file={}", path_arg(&config.uefi_vars)).into(),
        "-drive".into(), format!("if=none,id=disk0,format=qcow2,file={},discard=unmap,cache=writeback", path_arg(&config.disk)).into(),
        "-device".into(), "virtio-blk-pci,drive=disk0".into(),
        "-netdev".into(), "user,id=net0".into(),
        "-device".into(), "virtio-net-pci,netdev=net0".into(),
        "-device".into(), "ich9-intel-hda".into(),
        "-device".into(), "hda-duplex".into(),
        "-boot".into(), if installer { "once=d,menu=on".into() } else { "strict=on".into() },
    ]);

    if !config.shares.is_empty() {
        args.insert(0, "-object".into());
        args.insert(1, format!("memory-backend-memfd,id=mem,size={},share=on", config.ram).into());
        args.insert(2, "-numa".into());
        args.insert(3, "node,memdev=mem".into());
    } else {
        args.insert(0, "-m".into());
        args.insert(1, config.ram.clone().into());
    }

    if installer {
        args.extend(["-cdrom".into(), config.iso.as_os_str().into()]);
    }

    for (idx, share) in config.shares.iter().enumerate() {
        let socket = vm_dir.join(format!("virtiofs-{idx}.sock"));
        let _ = fs::remove_file(&socket);
        let virtiofsd = find_virtiofsd().ok_or_else(|| {
            AppError::Message("virtiofsd nicht gefunden".into())
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

    let status = status.map_err(|e| AppError::Message(format!("QEMU konnte nicht gestartet werden: {e}")))?;
    if !status.success() {
        return Err(AppError::Message(format!("QEMU wurde mit Status {status} beendet")));
    }

    Ok(())
}

fn spawn_virtiofsd(binary: &Path, socket: &Path, share: &Path) -> Result<Child> {
    let mut cmd = if unsafe { libc::geteuid() } == 0 {
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
        OsString::from("--sandbox"), OsString::from(if unsafe { libc::geteuid() } == 0 { "namespace" } else { "chroot" }),
        OsString::from("--cache"), OsString::from("auto"),
    ]);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AppError::Message(format!("virtiofsd konnte nicht gestartet werden: {e}")))?;

    thread::sleep(Duration::from_millis(100));
    if let Some(status) = child.try_wait()? {
        let mut msg = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut msg);
        }
        return Err(AppError::Message(format!(
            "virtiofsd ist sofort beendet ({status}): {}",
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
        "virtiofsd hat den Socket nicht bereitgestellt: {}",
        path.display()
    )))
}

fn stop_vm(data_dir: &Path, name: &str) -> Result<()> {
    let config = load_config(data_dir, name)?;
    let vm_dir = config_dir(&config)?;
    let pid_file = vm_dir.join("qemu.pid");
    if !pid_file.exists() {
        println!("VM `{name}` läuft nicht.");
        return Ok(());
    }
    let pid: i32 = fs::read_to_string(&pid_file)
        .map_err(AppError::from)?
        .trim()
        .parse()
        .map_err(|_| AppError::Message("ungültige QEMU PID-Datei".into()))?;

    if !is_pid_alive(pid) {
        let _ = fs::remove_file(&pid_file);
        println!("VM `{name}` lief nicht mehr; PID-Datei entfernt.");
        return Ok(());
    }

    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    if rc != 0 {
        return Err(AppError::Message(format!(
            "SIGTERM an QEMU PID {pid} fehlgeschlagen: {}",
            io::Error::last_os_error()
        )));
    }
    println!("SIGTERM an VM `{name}` gesendet (PID {pid}).");
    Ok(())
}

fn delete_vm(data_dir: &Path, name: &str) -> Result<()> {
    let config = load_config(data_dir, name)?;
    let vm_dir = config_dir(&config)?;
    if is_running(&vm_dir)? {
        return Err(AppError::Message(format!(
            "VM `{name}` läuft noch; zuerst `fedoravm stop {name}`"
        )));
    }
    fs::remove_dir_all(&vm_dir)?;
    println!("VM `{name}` gelöscht.");
    Ok(())
}

fn list_vms(data_dir: &Path) -> Result<()> {
    if !data_dir.exists() {
        println!("Keine VMs.");
        return Ok(());
    }
    let mut found = false;
    for entry in fs::read_dir(data_dir)? {
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
            "{:<20} {:<10} RAM={} CPU={} Shares={}",
            cfg.name,
            if running { "running" } else { "stopped" },
            cfg.ram,
            cfg.cpus,
            cfg.shares.len()
        );
    }
    if !found {
        println!("Keine VMs.");
    }
    Ok(())
}

fn doctor() -> Result<()> {
    println!("fedoravm doctor");
    println!("  Linux x86_64: {}", cfg!(target_os = "linux") && cfg!(target_arch = "x86_64"));
    report_binary("qemu-system-x86_64");
    report_binary("qemu-img");
    println!("  virtiofsd: {}", find_virtiofsd().map(|p| p.display().to_string()).unwrap_or_else(|| "NICHT GEFUNDEN".into()));
    println!("  /dev/kvm: {}", if Path::new("/dev/kvm").exists() { "vorhanden" } else { "fehlt" });
    match find_ovmf() {
        Ok((code, vars)) => println!("  OVMF: {} / {}", code.display(), vars.display()),
        Err(e) => println!("  OVMF: FEHLT ({e})"),
    }
    match qemu_has_venus() {
        Ok(true) => println!("  QEMU virtio-gpu Venus: unterstützt"),
        Ok(false) => println!("  QEMU virtio-gpu Venus: NICHT unterstützt"),
        Err(e) => println!("  QEMU virtio-gpu Venus: unbekannt ({e})"),
    }

    println!("\nHost-Hinweis: Venus benötigt einen passenden Vulkan-Treiber auf dem Linux-Host und die in Mesa/QEMU dokumentierten Kernel-/Mesa-Versionen.");
    println!("Für Fedora 44 ist virtiofsd als Paket verfügbar; OVMF kommt aus edk2-ovmf.");
    Ok(())
}

impl VmConfig {
    fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }
}

fn save_config(vm_dir: &Path, config: &VmConfig) -> Result<()> {
    let path = vm_dir.join("vm.json");
    fs::write(path, serde_json::to_vec_pretty(config)?)?;
    Ok(())
}

fn load_config(data_dir: &Path, name: &str) -> Result<VmConfig> {
    validate_name(name)?;
    let path = data_dir.join(name).join("vm.json");
    if !path.exists() {
        return Err(AppError::Message(format!(
            "VM `{name}` nicht gefunden"
        )));
    }
    VmConfig::load(&path)
}

fn config_dir(config: &VmConfig) -> Result<PathBuf> {
    config
        .disk
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| AppError::Message("VM-Konfiguration hat kein gültiges Verzeichnis".into()))
}

fn is_running(vm_dir: &Path) -> Result<bool> {
    let pid_file = vm_dir.join("qemu.pid");
    if !pid_file.exists() {
        return Ok(false);
    }
    let pid: i32 = fs::read_to_string(pid_file)
        .map_err(AppError::from)?
        .trim()
        .parse()
        .map_err(|_| AppError::Message("ungültige QEMU PID-Datei".into()))?;
    Ok(is_pid_alive(pid))
}

fn is_pid_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn validate_name(name: &str) -> Result<String> {
    if name.is_empty() || name.len() > 64 {
        return Err(AppError::Message("VM-Name muss 1..64 Zeichen lang sein".into()));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(AppError::Message(
            "VM-Name darf nur A-Z, a-z, 0-9, - und _ enthalten".into(),
        ));
    }
    Ok(name.to_string())
}

fn temp_name() -> String {
    format!("temp-{}", std::process::id())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn http_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent("fedoravm/0.1")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()?)
}

fn require_binary(name: &str) -> Result<()> {
    if find_binary(name).is_none() {
        return Err(AppError::Message(format!(
            "`{name}` wurde nicht gefunden"
        )));
    }
    Ok(())
}

fn report_binary(name: &str) {
    println!(
        "  {name}: {}",
        find_binary(name)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "NICHT GEFUNDEN".into())
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
        "/usr/libexec/virtiofsd",
        "/usr/lib/qemu/vhost-user/virtiofsd",
    ]
    .into_iter()
    .find_map(find_binary)
}

fn find_ovmf() -> Result<(PathBuf, PathBuf)> {
    let candidates = [
        ("/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd"),
        ("/usr/share/edk2/ovmf/OVMF_CODE_4M.fd", "/usr/share/edk2/ovmf/OVMF_VARS_4M.fd"),
        ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
    ];
    for (code, vars) in candidates {
        let c = PathBuf::from(code);
        let v = PathBuf::from(vars);
        if c.is_file() && v.is_file() {
            return Ok((c, v));
        }
    }
    Err(AppError::Message(
        "OVMF nicht gefunden; installiere edk2-ovmf".into(),
    ))
}

fn ensure_kvm_support() -> Result<()> {
    if !Path::new("/dev/kvm").exists() {
        return Err(AppError::Message(
            "/dev/kvm fehlt. Aktiviere KVM/Hardware-Virtualisierung auf dem Host.".into(),
        ));
    }
    Ok(())
}

fn ensure_venus_support() -> Result<()> {
    match qemu_has_venus()? {
        true => Ok(()),
        false => Err(AppError::Message(
            "das installierte QEMU kennt `virtio-gpu-gl,venus=true` nicht. Aktualisiere QEMU/virglrenderer.".into(),
        )),
    }
}

fn qemu_has_venus() -> Result<bool> {
    let output = Command::new("qemu-system-x86_64")
        .args(["-device", "virtio-gpu-gl,help"])
        .output()?;
    if !output.status.success() {
        return Ok(false);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.contains("venus"))
}

fn require_linux_x86_64() -> Result<()> {
    if !cfg!(target_os = "linux") || !cfg!(target_arch = "x86_64") {
        return Err(AppError::Message(
            "diese Version von fedoravm unterstützt derzeit Linux/x86_64 als Host".into(),
        ));
    }
    Ok(())
}

fn run_checked(command: &mut Command, what: &str) -> Result<()> {
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::Message(format!("{what}: Status {status}")))
    }
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn render_command(args: &[OsString]) -> String {
    args.iter()
        .map(|s| shell_quote(&s.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    if s.bytes().all(|b| {
        matches!(b,
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' |
            b'_' | b'-' | b'.' | b'/' | b':' | b',' | b'=' | b'+'
        )
    }) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}
