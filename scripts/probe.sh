#!/usr/bin/env bash
# Sandfork feasibility probe (Part 0).
#
# Runs every check Sandfork's design depends on and prints one JSON object
# for this machine. Needs no root unless --root is passed, which adds the
# CRIU and cgroup-controller checks. Nothing here changes the machine.
#
#   scripts/probe.sh            > probe.json
#   scripts/probe.sh --root     > probe.json     # uses sudo -n; skips if it would prompt
#   WORK=/path scripts/probe.sh                  # where Sandfork's state would live

set -u
ROOT=0
[ "${1:-}" = "--root" ] && ROOT=1
[ "${2:-}" = "--root" ] && ROOT=1
WORK="${WORK:-${XDG_DATA_HOME:-$HOME/.local/share}}"
mkdir -p "$WORK" 2>/dev/null || WORK=/tmp
T="$(mktemp -d "${TMPDIR:-/tmp}/sf-probe.XXXXXX")"
trap 'rm -rf "$T"' EXIT

json_escape() { printf '%s' "$1" | tr '\n\t' '  ' | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/[[:cntrl:]]//g'; }
FIRST=1
emit() { # key value(already JSON)
  if [ $FIRST -eq 1 ]; then FIRST=0; else printf ',\n'; fi
  printf '  "%s": %s' "$1" "$2"
}
str() { emit "$1" "\"$(json_escape "$2")\""; }
bool() { if [ "$2" = "1" ]; then emit "$1" true; else emit "$1" false; fi; }
run() { # run a command quietly with a timeout; echo its trimmed stdout
  timeout 20 "$@" 2>/dev/null | head -c 2000
}
ok() { timeout 20 "$@" >/dev/null 2>&1 && echo 1 || echo 0; }

printf '{\n'
str host "$(hostname 2>/dev/null)"
str date "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
str kernel "$(uname -r)"
str arch "$(uname -m)"
str distribution "$(. /etc/os-release 2>/dev/null; echo "${PRETTY_NAME:-unknown}")"
str virtualization "$( (grep -qi microsoft /proc/version && echo wsl2) || (test -f /.dockerenv && echo docker) || (systemd-detect-virt 2>/dev/null) || echo unknown)"

# Kernel 5.15 or newer has every primitive the design uses.
KMAJ="$(uname -r | cut -d. -f1)"; KMIN="$(uname -r | cut -d. -f2)"
if [ "$KMAJ" -gt 5 ] || { [ "$KMAJ" -eq 5 ] && [ "$KMIN" -ge 15 ]; }; then bool kernel_ok 1; else bool kernel_ok 0; fi

emit cores "$(nproc 2>/dev/null || echo 0)"
emit memory_mb "$(awk '/MemTotal/ {printf "%d", $2/1024}' /proc/meminfo)"
emit memory_available_mb "$(awk '/MemAvailable/ {printf "%d", $2/1024}' /proc/meminfo)"
str work_dir "$WORK"
str work_fs "$(stat -f -c %T "$WORK" 2>/dev/null)"
emit work_free_mb "$(df -Pm "$WORK" 2>/dev/null | awk 'NR==2 {print $4}')"
emit shm_mb "$(df -Pm /dev/shm 2>/dev/null | awk 'NR==2 {print $2}')"

# cgroup v2 and delegation.
CG="$(stat -fc %T /sys/fs/cgroup 2>/dev/null)"
str cgroup_fs "$CG"
bool cgroup_v2 "$([ "$CG" = cgroup2fs ] && echo 1 || echo 0)"
str cgroup_controllers "$(cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null)"
UIDN="$(id -u)"
str cgroup_delegated_controllers "$(cat "/sys/fs/cgroup/user.slice/user-$UIDN.slice/user@$UIDN.service/cgroup.controllers" 2>/dev/null)"
bool systemd "$([ "$(ps -p 1 -o comm= 2>/dev/null)" = systemd ] && echo 1 || echo 0)"
bool systemd_run_user "$(ok systemd-run --user --scope -q -p Delegate=yes true)"
for f in cgroup.kill cgroup.freeze; do :; done
bool cgroup_kill "$([ "$KMAJ" -gt 5 ] || { [ "$KMAJ" -eq 5 ] && [ "$KMIN" -ge 14 ]; } && echo 1 || echo 0)"
bool memory_peak "$([ "$KMAJ" -gt 5 ] || { [ "$KMAJ" -eq 5 ] && [ "$KMIN" -ge 19 ]; } && echo 1 || echo 0)"

# Namespaces. On Ubuntu 23.10+ an AppArmor restriction lets an unconfined
# program create a user namespace but strips its capabilities inside it.
str userns_uid "$(run unshare -Ur id -u)"
bool user_namespaces "$([ "$(run unshare -Ur id -u)" = 0 ] && echo 1 || echo 0)"
str apparmor_restrict_unprivileged_userns "$(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || echo absent)"
str unprivileged_userns_clone "$(sysctl -n kernel.unprivileged_userns_clone 2>/dev/null || echo absent)"
emit max_user_namespaces "$(cat /proc/sys/user/max_user_namespaces 2>/dev/null || echo 0)"
bool mount_namespace "$(ok unshare -Urm sh -c 'mount -t tmpfs none /mnt')"
bool pid_namespace "$([ "$(run unshare -Urpf --mount-proc sh -c 'echo $$')" = 1 ] && echo 1 || echo 0)"
str net_namespace_links "$(run unshare -Urn sh -c 'ip -br link 2>/dev/null || cat /proc/net/dev | tail -n +3 | cut -d: -f1' | tr -s ' \n' ' ')"
bool time_namespace "$([ -e /proc/self/ns/time ] && echo 1 || echo 0)"

# Subordinate IDs: with them a sandbox's root is a distinct host UID.
str subuid "$(grep -E "^($(id -un)|$UIDN):" /etc/subuid 2>/dev/null | head -1)"
bool newuidmap "$(command -v newuidmap >/dev/null 2>&1 && command -v newgidmap >/dev/null 2>&1 && echo 1 || echo 0)"

# overlayfs inside a user namespace, with user.overlay.* markers.
mkdir -p "$T/o/l" "$T/o/u" "$T/o/w" "$T/o/m"
bool overlay_in_userns "$(ok unshare -Urm sh -c "mount -t overlay overlay -o lowerdir=$T/o/l,upperdir=$T/o/u,workdir=$T/o/w,userxattr $T/o/m")"
mkdir -p "$WORK/.sf-probe/l" "$WORK/.sf-probe/u" "$WORK/.sf-probe/w" "$WORK/.sf-probe/m" 2>/dev/null
bool overlay_on_work_dir "$(ok unshare -Urm sh -c "mount -t overlay overlay -o lowerdir=$WORK/.sf-probe/l,upperdir=$WORK/.sf-probe/u,workdir=$WORK/.sf-probe/w,userxattr $WORK/.sf-probe/m")"
rm -rf "$WORK/.sf-probe" 2>/dev/null
# A tmpfs writable layer needs user xattrs on tmpfs (Linux 6.6).
bool tmpfs_user_xattr "$(ok unshare -Urm sh -c "mount -t tmpfs none $T/o/m && mkdir $T/o/m/d && setfattr -n user.overlay.opaque -v y $T/o/m/d 2>/dev/null || python3 -c \"import os; os.setxattr('$T/o/m/d', 'user.overlay.opaque', b'y')\"")"
bool whiteout_mknod "$(ok unshare -Urm sh -c "mknod $T/o/u/wh c 0 0")"
bool fuse_overlayfs "$(command -v fuse-overlayfs >/dev/null 2>&1 && test -c /dev/fuse && echo 1 || echo 0)"

str seccomp_state "$(awk '/^Seccomp:/ {print $2}' /proc/self/status)"
str cap_eff "$(awk '/^CapEff:/ {print $2}' /proc/self/status)"
str cap_bnd "$(awk '/^CapBnd:/ {print $2}' /proc/self/status)"
bool openat2 "$(python3 -c "
import ctypes, os, struct
libc = ctypes.CDLL(None, use_errno=True)
how = struct.pack('QQQ', os.O_RDONLY, 0, 0x08)
fd = libc.syscall(437, -100, b'.', how, len(how))
raise SystemExit(0 if fd >= 0 else 1)" >/dev/null 2>&1 && echo 1 || echo 0)"

# Checkpoint and restore.
CRIU="$(criu --version 2>/dev/null | head -1)"
str criu_version "${CRIU:-absent}"
bool checkpoint_restore_config "$([ -e /proc/sys/kernel/ns_last_pid ] && echo 1 || echo 0)"
bool soft_dirty "$( (zcat /proc/config.gz 2>/dev/null | grep -q 'CONFIG_MEM_SOFT_DIRTY=y') && echo 1 || echo 0)"
bool userfaultfd "$( (zcat /proc/config.gz 2>/dev/null | grep -q 'CONFIG_USERFAULTFD=y') && echo 1 || echo 0)"
if [ $ROOT -eq 1 ] && sudo -n true 2>/dev/null; then
  str criu_check "$(sudo -n criu check 2>&1 | tail -1)"
  bool criu_lazy_pages "$(sudo -n criu check --feature lazy_pages >/dev/null 2>&1 && echo 1 || echo 0)"
  bool criu_mem_dirty_track "$(sudo -n criu check --feature mem_dirty_track >/dev/null 2>&1 && echo 1 || echo 0)"
  bool root_checks 1
else
  bool root_checks 0
fi

bool kvm "$(test -r /dev/kvm -a -w /dev/kvm && echo 1 || echo 0)"
bool kvm_present "$(test -e /dev/kvm && echo 1 || echo 0)"
bool pss_readable "$(test -r /proc/self/smaps_rollup && echo 1 || echo 0)"
str ksm_run "$(cat /sys/kernel/mm/ksm/run 2>/dev/null || echo absent)"
str thp "$(cat /sys/kernel/mm/transparent_hugepage/enabled 2>/dev/null || echo absent)"
echo data > "$WORK/.sf-reflink-a" 2>/dev/null
bool reflink "$(ok cp --reflink=always "$WORK/.sf-reflink-a" "$WORK/.sf-reflink-b")"
rm -f "$WORK/.sf-reflink-a" "$WORK/.sf-reflink-b"

# Toolchains.
str rustc "$(rustc --version 2>/dev/null || "$HOME/.cargo/bin/rustc" --version 2>/dev/null || echo absent)"
str python "$(python3 --version 2>/dev/null || echo absent)"
str gcc "$(gcc --version 2>/dev/null | head -1 || echo absent)"
bool hyperfine "$(command -v hyperfine >/dev/null 2>&1 && echo 1 || echo 0)"
str gpu "$(nvidia-smi --query-gpu=name,memory.total --format=csv,noheader 2>/dev/null | tr '\n' ';' || true)"

# Which isolation levels this machine can run.
L1=0
[ "$(run unshare -Ur id -u)" = 0 ] && [ "$(ok unshare -Urm sh -c "mount -t overlay overlay -o lowerdir=$T/o/l,upperdir=$T/o/u,workdir=$T/o/w,userxattr $T/o/m")" = 1 ] && L1=1
bool level_l0 1
bool level_l1 "$L1"
bool level_l2_possible "$([ $L1 -eq 1 ] && [ "$CG" = cgroup2fs ] && echo 1 || echo 0)"
printf '\n}\n'
