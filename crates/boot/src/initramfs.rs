//! Building the recovery system's initramfs from the running system: Kitten's static binary
//! as `/init`, the filesystem tools with their shared libraries, and the kernel modules the
//! recovery system loads. Everything comes from the user's own distro, so nothing Kitten
//! ships needs signing and the modules always match the kernel.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use bootshift_fs::run::{CommandLine, RunError, Runner};

use crate::cpio::CpioWriter;

/// Tools the recovery system can't work without.
pub const REQUIRED_TOOLS: &[&str] = &[
    "e2fsck",
    "resize2fs",
    "dumpe2fs",
    "tune2fs",
    "ntfsresize",
    "ntfscat",
    "mkswap",
    "blkid",
    "partx",
    "mount",
    "umount",
    "efibootmgr",
    "modprobe",
];

/// Tools for filesystems a system may not have installed; included when present.
pub const OPTIONAL_TOOLS: &[&str] = &["btrfs", "xfs_growfs", "xfs_repair"];

/// Storage, filesystem, firmware and keyboard drivers, plus virtio for test machines. Modules
/// that are built in or missing from the kernel are skipped.
pub const MODULES: &[&str] = &[
    "nvme",
    "vmd",
    "ahci",
    "sd_mod",
    "virtio_blk",
    "virtio_pci",
    "ext4",
    "vfat",
    "nls_cp437",
    "nls_iso8859_1",
    "btrfs",
    "xfs",
    "efivarfs",
    "xhci_pci",
    "usbhid",
    "hid_generic",
    "i8042",
    "atkbd",
];

/// Where the recovery system finds the list of modules to load.
pub const MODULE_LIST_PATH: &str = "/etc/bootshift/modules";

#[derive(Debug, thiserror::Error)]
pub enum InitramfsError {
    #[error(transparent)]
    Tool(#[from] RunError),
    #[error("can't read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the recovery system needs `{0}`, which isn't installed")]
    MissingTool(String),
}

pub struct Inputs<'a> {
    pub init: &'a Path,
    pub kernel_release: &'a str,
    pub search_path: &'a [PathBuf],
}

/// Builds the uncompressed cpio archive.
pub fn build_archive(runner: &dyn Runner, inputs: &Inputs<'_>) -> Result<Vec<u8>, InitramfsError> {
    let mut archive = Archive::default();
    for directory in ["dev", "proc", "sys", "run", "tmp", "mnt", "etc/bootshift"] {
        archive.directory(directory);
    }
    archive.writer.char_device("/dev/console", 0o600, 5, 1);
    archive.file_from("/init", inputs.init, 0o755)?;

    let mut binaries = Vec::new();
    for name in REQUIRED_TOOLS {
        binaries.push(
            find_program(name, inputs.search_path)
                .ok_or_else(|| InitramfsError::MissingTool(name.to_string()))?,
        );
    }
    binaries.extend(
        OPTIONAL_TOOLS
            .iter()
            .filter_map(|n| find_program(n, inputs.search_path)),
    );
    for binary in &binaries {
        archive.copy(binary, 0o755)?;
        for library in shared_libraries(runner, binary)? {
            archive.copy(&library, 0o755)?;
        }
    }

    let modules_dir = Path::new("/lib/modules").join(inputs.kernel_release);
    let mut loadable = Vec::new();
    for module in MODULES {
        let files = module_files(runner, inputs.kernel_release, module)?;
        if !files.is_empty() {
            loadable.push(*module);
        }
        for file in files {
            archive.copy(&file, 0o644)?;
        }
    }
    for index in module_indexes(&modules_dir)? {
        archive.copy(&index, 0o644)?;
    }
    let list = loadable.join("\n") + "\n";
    archive
        .writer
        .file(MODULE_LIST_PATH, 0o644, list.as_bytes());

    Ok(archive.writer.finish())
}

/// Compresses an archive with zstd, which the distro kernels Kitten supports unpack.
pub fn compress(runner: &dyn Runner, archive: &Path, output: &Path) -> Result<(), RunError> {
    let command = CommandLine::new("zstd")
        .arg("-q")
        .arg("-19")
        .arg("-f")
        .path(archive)
        .arg("-o")
        .path(output);
    runner.run_ok(&command).map(|_| ())
}

pub fn find_program(name: &str, search_path: &[PathBuf]) -> Option<PathBuf> {
    search_path
        .iter()
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The shared libraries `binary` loads, including the dynamic loader. Empty for static
/// binaries.
///
/// A non-zero `ldd` exit is only taken to mean "static" when `ldd` actually says so
/// ("not a dynamic executable" / "statically linked"). Any other failure (ldd missing, a
/// wrong-architecture or unreadable binary) is ambiguous and fails closed with an error, rather
/// than being mistaken for a static binary — otherwise a dynamic init or tool could slip into the
/// recovery image with its libraries silently dropped, and fail to exec at boot.
pub fn shared_libraries(runner: &dyn Runner, binary: &Path) -> Result<Vec<PathBuf>, RunError> {
    let command = CommandLine::new("ldd").path(binary);
    let output = runner.run(&command)?;
    if output.status == Some(0) {
        return Ok(parse_ldd(&output.text()));
    }
    // ldd prints "not a dynamic executable" to STDERR (and some builds "statically linked" to
    // stdout), so check both streams. Only that marker means "static" (no libraries); any other
    // non-zero exit (ldd missing, wrong architecture, unreadable) is ambiguous and fails closed,
    // rather than being mistaken for a static binary and dropping its libraries from the image.
    let stdout = output.text();
    let is_static = ["not a dynamic executable", "statically linked"]
        .iter()
        .any(|&marker| stdout.contains(marker) || output.stderr.contains(marker));
    if is_static {
        return Ok(Vec::new());
    }
    Err(RunError::UnexpectedOutput {
        command: command.to_string(),
        detail: format!(
            "ldd failed and did not report a static binary: {} {}",
            stdout.trim(),
            output.stderr.trim()
        ),
    })
}

pub fn parse_ldd(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let path = match line.split_once("=>") {
                Some((_, rest)) => rest.trim(),
                None => line,
            };
            let path = path.split_whitespace().next()?;
            path.starts_with('/').then(|| PathBuf::from(path))
        })
        .collect()
}

/// The module files needed to load `module` on `kernel_release`, dependencies first. Empty
/// when the module is built in or doesn't exist for this kernel.
pub fn module_files(
    runner: &dyn Runner,
    kernel_release: &str,
    module: &str,
) -> Result<Vec<PathBuf>, RunError> {
    let command = CommandLine::new("modprobe")
        .arg("--show-depends")
        .arg("--set-version")
        .arg(kernel_release)
        .arg(module);
    let output = runner.run(&command)?;
    if output.status != Some(0) {
        return Ok(Vec::new());
    }
    Ok(parse_show_depends(&output.text()))
}

pub fn parse_show_depends(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("insmod "))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(PathBuf::from)
        .collect()
}

/// `modules.dep`, `modules.alias` and the other index files `modprobe` reads.
fn module_indexes(modules_dir: &Path) -> Result<Vec<PathBuf>, InitramfsError> {
    let read_error = |source| InitramfsError::Read {
        path: modules_dir.to_path_buf(),
        source,
    };
    let mut indexes = Vec::new();
    for entry in fs::read_dir(modules_dir).map_err(read_error)? {
        let path = entry.map_err(read_error)?.path();
        let is_index = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("modules."));
        if is_index && path.is_file() {
            indexes.push(path);
        }
    }
    indexes.sort();
    Ok(indexes)
}

/// A cpio writer that adds each file's parent directories once.
#[derive(Default)]
struct Archive {
    writer: CpioWriter,
    added: BTreeSet<String>,
}

impl Archive {
    fn directory(&mut self, path: &str) {
        let path = path.trim_matches('/');
        let mut prefix = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if self.added.insert(prefix.clone()) {
                self.writer.directory(&prefix, 0o755);
            }
        }
    }

    /// Copies a host file to the same path in the archive, following symlinks for its contents.
    fn copy(&mut self, path: &Path, permissions: u32) -> Result<(), InitramfsError> {
        let name = path.to_string_lossy().trim_start_matches('/').to_string();
        if self.added.contains(&name) {
            return Ok(());
        }
        self.file_from(&name, path, permissions)
    }

    fn file_from(
        &mut self,
        name: &str,
        source: &Path,
        permissions: u32,
    ) -> Result<(), InitramfsError> {
        let name = name.trim_start_matches('/');
        if let Some((parent, _)) = name.rsplit_once('/') {
            self.directory(parent);
        }
        let contents = fs::read(source).map_err(|source_error| InitramfsError::Read {
            path: source.to_path_buf(),
            source: source_error,
        })?;
        self.writer.file(name, permissions, &contents);
        self.added.insert(name.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ldd_output_gives_library_and_loader_paths() {
        let text = "\tlinux-vdso.so.1 (0x00007ffc)\n\
                    \tlibext2fs.so.2 => /lib/x86_64-linux-gnu/libext2fs.so.2 (0x00007f00)\n\
                    \tlibc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x00007f01)\n\
                    \t/lib64/ld-linux-x86-64.so.2 (0x00007f02)\n";
        assert_eq!(
            parse_ldd(text),
            [
                PathBuf::from("/lib/x86_64-linux-gnu/libext2fs.so.2"),
                PathBuf::from("/lib/x86_64-linux-gnu/libc.so.6"),
                PathBuf::from("/lib64/ld-linux-x86-64.so.2"),
            ]
        );
    }

    #[test]
    fn a_static_binary_is_reported_as_having_no_libraries() {
        use bootshift_fs::run::ScriptedRunner;
        // ldd prints the "not a dynamic executable" marker to stderr with a non-zero exit.
        let runner = ScriptedRunner::new().reply_error(1, "\tnot a dynamic executable\n");
        assert_eq!(
            shared_libraries(&runner, Path::new("/usr/libexec/bootshift/bootshift")).ok(),
            Some(Vec::<PathBuf>::new()),
        );
    }

    #[test]
    fn an_ambiguous_ldd_failure_fails_closed() {
        use bootshift_fs::run::ScriptedRunner;
        // ldd missing / wrong arch: no "not a dynamic executable" marker, so it must error rather
        // than be mistaken for a static binary.
        let runner = ScriptedRunner::new().reply_error(127, "ldd: command not found");
        assert!(shared_libraries(&runner, Path::new("/usr/libexec/bootshift/bootshift")).is_err());
    }

    #[test]
    fn show_depends_lists_modules_and_skips_builtins() {
        let text = "insmod /lib/modules/6.8.0/kernel/lib/crc16.ko.zst \n\
                    insmod /lib/modules/6.8.0/kernel/fs/ext4/ext4.ko.zst \n\
                    builtin vfat\n";
        assert_eq!(
            parse_show_depends(text),
            [
                PathBuf::from("/lib/modules/6.8.0/kernel/lib/crc16.ko.zst"),
                PathBuf::from("/lib/modules/6.8.0/kernel/fs/ext4/ext4.ko.zst"),
            ]
        );
    }
}
