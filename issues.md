# Known issues

What is wrong, missing or misleading in the code right now, found in the code audit of
2026-10-26. `task.md` stays the ordered work plan; each issue here names the task that closes
it. Remove an issue in the same change that fixes it.

## Release blockers

- **I-03 Guarded runs have no kernel transport.** The undo-log core (`disk::guard`,
  `disk::UndoLog`, `disk::roll_back`) is crash-tested, but nothing connects `resize2fs` or
  `ntfsresize` to it, so at runtime ext4/NTFS shrink and NTFS grow refuse with "this needs a
  guarded run, which this build doesn't have yet". Until it lands, Kitten can move anything
  and grow ext4/Btrfs/XFS, but can't take space *from* an NTFS or ext4 partition. Tasks 0.6, 7.3.
- **I-06 Windows survival is unproven.** Nobody has verified that Windows 11 boots after its
  partition moves, or that WinRE survives a recovery-partition move. This is the project's
  biggest external risk. Tasks 0.2, 0.3.

## Degraded behaviour

- **I-04 The desktop app has never talked to a live helper.** `bootshift-gui` (`crates/gui`)
  has a zbus client for `org.bootshift.Helper1`, but it has only run against the demo backend;
  nothing has called a running `bootshiftd` yet. Tasks 13.5, 13.6.
- **I-18 The helper's `Check` doesn't list its checks.** It answers `null` when every check
  passes, while the GUI's contract is one row per check. The app then shows no rows and keeps
  apply locked, which is safe but means apply can never be reached against the real helper.
  `Check` should return the rows from `engine::preflight::evaluate`. Task 13.6.
- **I-34 Preflight doesn't check for the Btrfs and XFS tools.** `unavailable_operations`
  allows a Btrfs shrink or grow and an XFS grow by filesystem alone, while `btrfs`,
  `xfs_growfs` and `xfs_repair` are `OPTIONAL_TOOLS` that the initramfs skips when missing. On a
  system without them the resize passes Apply and fails after the restart; the journal keeps
  the data safe, but the user has to recover. The `.deb` and `.rpm` depend on both packages, so
  this only bites the CLI tarball and source builds. Preflight should refuse when a planned
  operation's tool is missing. Task 9.1.
- **I-08 systemd-boot is not supported yet**, though section 0 scopes it in (without Secure
  Boot). Preflight refuses it. Task 10.5.
## Paper cuts

- **I-12 No cancel or undo.** Task 8.6.
- **I-13 No dm-linear staging check** before the first table write. Task 8.3.
- **I-14 The GUI stops at "restart later".** The restart screen and post-restart summary now
  exist in the frontend with a demo simulation, but nothing reads the real journal for the
  summary, and the table-backup download is missing. Tasks 13.3, 13.4.
- **I-15 The packages are untested, and there is no `docs/design.md` or demo recording.**
  `release.yml` builds a `.deb` and `.rpm`, but neither has been installed on a fresh system
  yet. Tasks 14.2, 14.3, 14.4.

## Fixed

- **I-32 Deleting a partition always failed in recovery and left a boot loop.** Fixed
  2026-10-29, found on a real dual-boot laptop. The executor never told the table writer that
  deleted partitions may go, so the first table write refused (`UnknownPartition`); recovery
  then powered off with the boot redirect in place, and every start returned to the error
  screen. Deleted partitions are now removable, and every table a plan will write is built in
  memory at Apply and again in recovery before the first write. A plan that can't be carried
  out is refused there, normal booting is restored, and the computer restarts into its system.
- **I-33 A failing tool's explanation was blank.** Fixed 2026-10-29: `ntfsresize` explains on
  stdout, so the refusal showed only "exit code Some(1):". Failures now show the tool's stderr,
  or its last stdout lines when stderr is empty, and a plain exit code.
- **I-02 The boot path had never run.** Retired 2026-10-26: `cargo xtask boot-proof` runs the
  whole chain in a QEMU/OVMF guest on its own SATA disk — `apply`, reboot into recovery as
  PID 1, a real ext4 partition moved, **the VM killed mid-move at ~9% (a true power cut)**, the
  resume from the journal, and verification that every one of 1.2 GB of file hashes matches,
  the table is on target, the GRUB redirect is gone and the boot order restored. Bugs this
  flushed out along the way, none in the safety core: SMART refused on QEMU's NVMe (switched
  the guest to SATA); `filefrag` returns an empty table on vfat (added a FIBMAP ioctl path,
  `disk::fibmap`); `/dev/kmsg` needs one write per record (recovery messages were blank);
  plus harness-only fixes (growpart, umount-busy, stale binary, serial strings).
- **I-01 `xtask` was an empty stub.** Fixed 2026-10-26: `cargo xtask prepare-vm`, `vm-test`
  and `boot-proof` are real (QEMU + OVMF guest, NVMe disk, cloud-init). The nightly
  `crash-suite` is still to come (Phase 11).
- **I-05 No progress reporting.** Fixed 2026-10-26: the engine has a `ProgressSink`
  (operation + bytes-copied callbacks on every committed chunk), and recovery prints the
  operation and a line every five percent. Time-left is still to do (task 10.3).
- **I-07 `accept_unreadable` was unreachable.** Fixed 2026-10-26: `bootshift resume
  --accept-unreadable`, and the recovery error screen names the command when a read fails.
- **I-09 The recovery error screen waited for Enter forever.** Fixed 2026-10-26: it powers
  off by itself after five minutes.
- **I-10 The helper's work directory could collide.** Fixed 2026-10-26: one directory per
  apply call, removed afterwards.
- **I-11 Btrfs and XFS moves copied every byte.** Fixed 2026-10-26: Btrfs used ranges come
  from the dev-extent tree (`btrfs inspect-internal dump-tree -t dev`), XFS as the complement
  of `xfs_db "freesp -d"`. Multi-device Btrfs, realtime XFS and external logs fall back to a
  full copy on purpose.
- **I-17 `probe` failed on util-linux older than 2.37** (Ubuntu 20.04 and kin): the PARTN,
  START and MOUNTPOINTS columns don't exist there, partitions come flat, and older lsblk
  quotes numbers. Fixed 2026-10-26: a compat column set, flat grouping, numbers from device
  names, start sectors from `sfdisk --json`, forgiving number/bool parsing.
- **I-16 `CLAUDE.md` told contributors to run commands that don't exist** (`cargo xtask …`,
  see I-01). Fixed 2026-10-26: the commands are now the real ones.
