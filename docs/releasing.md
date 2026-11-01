# Releasing Kitten

A release is one `kitten` package per distro family plus a static CLI:

| File | For |
|---|---|
| `kitten_<version>_amd64.deb` | Ubuntu, Debian, Mint, Pop!_OS |
| `kitten-<version>-1.x86_64.rpm` | Fedora, openSUSE |
| `kitten-cli-<version>-x86_64-linux-musl.tar.gz` | The `bootshift` CLI alone, e.g. to run `bootshift resume` from a live USB |
| `SHA256SUMS` | Checksums of the files above |

The package installs the GUI, the `bootshiftd` helper, the static `bootshift` binary the recovery
boot uses, and the D-Bus, polkit and systemd files. It pulls in the filesystem tools as
dependencies. `packaging/nfpm.yaml` lists every file and dependency.

## Steps

1. Make sure CI is green on `main`, and run the end-to-end proof locally:

   ```bash
   cargo xtask boot-proof
   ```

2. Set the version in the root `Cargo.toml` (`[workspace.package] version`), then commit:

   ```bash
   cargo check --workspace          # updates Cargo.lock
   git commit -am "repo: release 0.1.0-alpha.1"
   git push origin main
   ```

3. Tag that commit and push the tag. The tag must be `v` + the Cargo version, or the build stops.

   ```bash
   git tag -a v0.1.0-alpha.1 -m "Kitten 0.1.0-alpha.1"
   git push origin v0.1.0-alpha.1
   ```

4. `.github/workflows/release.yml` runs the checks, builds the packages and creates a **draft**
   release. A version with a `-` (like `-alpha.1`) is marked as a pre-release.

5. Before publishing the draft, install the `.deb` on a fresh Ubuntu VM and the `.rpm` on a fresh
   Fedora VM. Open Kitten from the app menu, check that it lists the disks, and run one move through
   restart and recovery. Then edit the release notes and press **Publish**.

## Release notes

Start with who should and shouldn't use this build. Until the real-hardware gate in `task.md`
(15.2) passes, every release is a pre-release and the notes say so, along with "back up your data
first" and which distros, filesystems and boot setups were tested.
