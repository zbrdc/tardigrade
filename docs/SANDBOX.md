# The OS sandbox (--sandbox)

Without `--sandbox`, tardigrade isolates guests by omission: the guest has no
host APIs, but it runs inside an ordinary process, and a bug in V8 or in
tardigrade's host code would act with your user's full rights. For hostile code,
`--sandbox` adds kernel-level containment around the whole process.

It is available on Linux x86_64 only; the deny list and the x32 guard are
verified on that ABI. On any other platform, or on a kernel missing a
required feature, tardigrade exits 2 without running the guest. It never falls
back to running unsandboxed. `tardigrade --sandbox-check` tells you in advance
whether it can work (exit 0) or not (exit 2).

## When the layers are applied

The layers are applied after tardigrade reads the entry source and the preload
file, and before the guest starts. V8 is initialized afterwards on a host
thread that the first run starts, so V8's worker threads, every guest
thread, the watchdog and the stdin reader all inherit the layers, and so
does every later run in the same process. The library refuses to apply the
layers once that host thread exists.

Before installing the filters, tardigrade closes every inherited file descriptor
numbered 3 or higher (anything passed in would be a channel out of the
sandbox) and sets `PR_SET_NO_NEW_PRIVS`. The seccomp filters are installed
with `TSYNC`, so they cover every thread.

## Landlock

- Filesystem access is denied except for two files. The `--dump` file is
  created, without truncation, when the sandbox starts; it is the only file
  the run can write. The end-of-run snapshot truncates and rewrites it in
  place (the atomic temporary-file replace used without `--sandbox` cannot
  create a file here), and even reading it back is denied. A run that ends
  before the snapshot leaves an existing file untouched, or an empty new
  file.
- `--dump` must name a regular file. One that cannot be opened for writing
  is refused with "cannot prepare dump"; one that opens but is not a
  regular file (a pipe, or `--dump=/dev/stdout` with stdout piped) is
  refused with "--dump must be a regular file", because Landlock can anchor
  its rule only on a file or directory inode.
- `/dev/urandom` is readable, because V8 opens it to seed its random number
  generators.
- Landlock also scopes the process itself (ABI 6): it may not send signals
  to processes outside the sandbox or connect to abstract unix sockets
  outside it. This is the layer that stops a guest from signalling another
  process of the same user.

Landlock does not mediate file metadata, file existence or symlink targets:
these stay observable through `stat`, `access`, `readlink`, `chdir` and
`statfs`. Extended-attribute calls are denied by seccomp instead.

## seccomp

A deny list. Denied calls fail with `EPERM`:

- all socket calls
- program execution and process creation: `execve`, `execveat`, `fork`,
  `vfork`
- `ptrace` and cross-process memory access
- process comparison and pidfds: `kcmp`, `pidfd_open`, `pidfd_getfd`,
  `pidfd_send_signal`
- privilege and identity changes (including `capset`), mount and
  namespace calls, `chroot` and `pivot_root`, kernel modules, `bpf`,
  `perf`, `io_uring`, `userfaultfd`, `keyctl`, `add_key`, `request_key`,
  and installing further seccomp filters
- system administration: `reboot`, `kexec_load`, `kexec_file_load`,
  `swapon`, `swapoff`, `acct`, `sethostname`, `setdomainname`, `syslog`,
  `ioperm`, `iopl`, `uselib`
- every System V IPC family (shared memory, semaphores, message queues)
  and every POSIX message-queue call
- `memfd_create`, `memfd_secret`, `personality`, `modify_ldt`
- extended attributes and other file metadata changes
- file-event notification (`inotify`, `fanotify`)
- fd-to-fd plumbing: `sendfile`, `splice`, `vmsplice`, `tee`,
  `copy_file_range`
- file handles: `name_to_handle_at`, `open_by_handle_at`
- setting clocks

Calls that can reach another process are filtered by argument, in addition
to the Landlock scopes:

- `kill`, `tgkill`, `rt_sigqueueinfo` and `rt_tgsigqueueinfo` are denied
  unless the target is the run's own pid. `tkill`, which names no process,
  is denied outright.
- `fcntl` is denied for `F_SETOWN`, `F_SETOWN_EX` and `F_SETSIG`, and
  `ioctl` for `FIOSETOWN` and `SIOCSPGRP`, the ways an async descriptor
  would aim `SIGIO` at another process. Landlock's signal scope blocks that
  delivery anyway; these denials are defense in depth.
- `prlimit64`, `sched_setaffinity`, `sched_setscheduler`, `sched_setattr`
  and `sched_setparam` are denied unless the target is 0 (the caller) or
  the run's own pid.
- `setpriority` and `ioprio_set` are denied unless they address a single
  process (`PRIO_PROCESS`, `IOPRIO_WHO_PROCESS`) that is 0 or the run's own
  pid.
- `ioctl` is denied for the terminal input injection requests `TIOCSTI` and
  `TIOCLINUX`. Other `ioctl` requests remain allowed on already-open
  descriptors, which is a residual surface.

Process creation is filtered, not only denied: `clone` succeeds only with
`CLONE_THREAD` set and no `CLONE_NEW*` namespace flag, and `clone3` fails
with `ENOSYS` so that glibc falls back to `clone`, which is then checked.
Threads still work; fork and user-namespace escapes do not.

A second, hand-written filter kills the process on any syscall made through
another ABI: a non-x86_64 architecture value (`int 0x80`) or a syscall
number in the x32 range, which would otherwise bypass every deny rule. The
same filter makes every syscall numbered 452 (`fchmodat2`) or higher fail
with `ENOSYS`, so syscalls added by future kernels cannot bypass the deny
list the way `fchmodat2` and the `*xattrat` family once could.

## What stays allowed

Everything the runtime needs: threads, `mmap` and `mprotect` for the JIT,
`futex`, `nanosleep`, clocks, `getrandom`, and reads and writes on
descriptors that are already open. The heap cap and the wall-clock timeout
keep working: the watchdog thread is created after the layers are applied,
with `clone(CLONE_THREAD)`, and needs only `futex` and `nanosleep`.

At startup V8 reads `/sys/devices/system/cpu/online` and `/proc/self/maps`,
and when the first is denied the C library tries `/proc/stat` instead. All
are denied under `--sandbox`, and V8 falls back: it sizes its worker pool
from `sched_getaffinity`.

## What it does not protect against

It is not a virtual machine. The guest runs as your user inside your
process. Out of scope:

- kernel bugs and hardware side channels
- the stdio descriptors and the `--dump` file itself
- CPU use until `--timeout`
- the shape of the filesystem around the process: `stat`, `access`,
  `readlink`, `chdir` and `statfs` are not mediated by Landlock
- `getpriority` and `sched_getaffinity` on other processes, which remain
  readable

The process's own pid is a valid signal target by design, because `raise`
and `abort` need it.

## Kernel requirements

Landlock ABI 6 (Linux 6.12) for the filesystem rights and the signal and
abstract-socket scopes, and seccomp with `TSYNC`. A ruleset that the kernel
cannot fully enforce is refused, not weakened. If Landlock or seccomp is
missing, tardigrade exits 2 with a message naming the layer. Verified on Linux
6.17 (Landlock ABI 7); older kernels are untested.

On other platforms `--sandbox` exits 2 with "--sandbox is only supported on
Linux x86_64".

How the sandbox tests behave on kernels without support is described in
[CONTRIBUTE.md](CONTRIBUTE.md#sandbox-tests).
