#!/bin/sh
# Boot entry point for the Rust camera stack.
#
# Called from the SD card, the same way gergehappy.sh is. POSIX sh only: the
# firmware provides BusyBox, not bash.
#
#   sh /mnt/disc1/anyka_rust/init.sh          start in the background and return
#   sh /mnt/disc1/anyka_rust/init.sh --fg     run in the foreground (serial console)
#   sh /mnt/disc1/anyka_rust/init.sh --check  validate everything and exit
#   sh /mnt/disc1/anyka_rust/init.sh --stop   stop a running stack
#
# Settings come from camera.conf beside this file (see camera.conf.example).
#
# Design rule for this file: **it must never be able to brick the camera.**
# Every step either succeeds or is skipped with a logged reason. Nothing here
# modifies the read-only rootfs or exits in a way that stops /etc/init.d/rcS
# from finishing. The vendor IPC process is stopped (stop_vendor_ipc=1, the
# default) only once preflight has proven the supervisor and its watchdog
# feeder can start, and immediately before starting them.
set -u

# `dirname` is not present in every BusyBox build on these cameras, so the
# directory is taken with parameter expansion instead.
case "$0" in
    */*) _self=${0%/*} ;;
    *)   _self=. ;;
esac
SELF_DIR=$(CDPATH= cd -- "$_self" 2>/dev/null && pwd) || SELF_DIR=/mnt/disc1/anyka_rust
BASE=$SELF_DIR

LOG_DIR=$BASE/logs
INIT_LOG=$LOG_DIR/init.log
PID_FILE=/tmp/anyka_rust.pid
# The restart loop runs in its own shell; it has to be stopped before the
# supervisor, or it would just start a new one.
LOOP_PID_FILE=/tmp/anyka_rust.loop.pid
SUPERVISOR=$BASE/bin/supervisor
CONFIG=$BASE/supervisor.toml
CAMERA_CONF=$BASE/camera.conf
SSHD=$BASE/bin/sshd
SSH_PASSWD=$BASE/etc/anyka-sshd.passwd
WEB=$BASE/bin/web
WEB_USERS=$BASE/etc/web-users
ONVIF_SECRET=$BASE/etc/onvif.secret
FALLBACK_FEEDER=$BASE/bin/wdt_feeder
# Vendor processes stopped when stop_vendor_ipc=1, matched against
# /proc/<pid>/comm. The watchers that respawn `ipc` or reboot the board when it
# disappears go first, then the IPC binary itself.
# `main` and `anyka_ipc` are what the AK3918AV100 firmware runs (the old
# quite_console.sh killed exactly those two); the rest are from other builds.
#
# This literal is the fallback: resolve_board replaces it with the board
# profile's [platform] vendor_processes (boards/<name>.toml), and keeps it
# whenever the board cannot be resolved. It must stay identical to
# boards/anyka-av100-ptz.toml, so a broken boards/ on the AV100 changes
# nothing.
DEFAULT_VENDOR_PROCESSES="daemon gwellupdater.sh soft_reboot.sh ipc anyka_ipc main"
VENDOR_PROCESSES=$DEFAULT_VENDOR_PROCESSES
# Board facts from `supervisor board --shell`; these are the fallbacks.
KAMODER_BOARD_NAME=
# Whether the board's vendor app holds /dev/watchdog at boot. Logged only:
# wdt_feeder always gets -w (harmless where nothing holds the device).
WATCHDOG_WAIT=1
# The vendor stack starts *after* the SD hook on the AV100, so a one-shot kill
# at boot finds nothing. A guard loop keeps killing it; this is its pid file.
GUARD_PID_FILE=/tmp/anyka_rust.guard.pid
# Seconds between guard scans of /proc.
GUARD_INTERVAL=3

# Keep this many rotated generations of each log file.
LOG_KEEP=5
# Rotate at 1 MiB.
LOG_MAX_BYTES=1048576
# Give up restarting the supervisor after this many failures in a row.
MAX_SUPERVISOR_RESTARTS=10

# --------------------------------------------------------------------------
# Logging
# --------------------------------------------------------------------------

# Milliseconds since boot. The camera has no RTC, so a wall clock would read
# 1970 until NTP lands.
uptime_stamp() {
    read -r up _ < /proc/uptime 2>/dev/null || up="?"
    printf '[%s]' "$up"
}

log() {
    printf '%s init: %s\n' "$(uptime_stamp)" "$*"
}

# Size-based rotation for this script's own log, done once at startup.
#
# The Rust binaries rotate their logs continuously through telemetry;
# shell cannot do that safely for a file it is currently appending to, so this
# rotates before the redirect and then leaves the file alone.
rotate() {
    file=$1
    [ -f "$file" ] || return 0
    size=$(wc -c < "$file" 2>/dev/null) || return 0
    [ "$size" -lt "$LOG_MAX_BYTES" ] && return 0

    i=$LOG_KEEP
    while [ "$i" -gt 1 ]; do
        prev=$((i - 1))
        [ -f "$file.$prev" ] && mv -f "$file.$prev" "$file.$i"
        i=$prev
    done
    mv -f "$file" "$file.1"
}

# --------------------------------------------------------------------------
# Environment
# --------------------------------------------------------------------------

# Libraries every binary loads from the card rather than the firmware: empty
# stubs for the libc pieces the crosstool (uClibc 0.9.33) splits out but the
# camera's uClibc-ng 1.0.31 merges into libc.so.0, and the GCC runtime. Each
# binary and SDK object finds them through its own RPATH, so nothing here sets
# LD_LIBRARY_PATH or touches /tmp. See sdk/runtime/README.md in the source.
RUNTIME_LIBS="libpthread.so.0 libm.so.0 librt.so.0 libdl.so.0 libgcc_s.so.1 libstdc++.so.6"

check_runtime_libs() {
    missing=""
    for lib in $RUNTIME_LIBS; do
        [ -f "$BASE/lib/sys/$lib" ] || missing="$missing $lib"
    done
    [ -z "$missing" ] && return 0
    log "ERROR: $BASE/lib/sys is incomplete, missing:$missing (rebuild the card with ./x sd)"
    return 1
}

# Mount the SD card if something else has not already done it.
#
# On the AK3918AV100 `/mnt` is a 16 MB tmpfs and the card belongs on
# `/mnt/disc1`; mounting over `/mnt` would hide whatever the firmware keeps
# there. `/mnt` is only a fallback for the older EV200 layout.
ensure_sd_mounted() {
    [ -d "$BASE" ] && return 0
    for mountpoint in /mnt/disc1 /mnt; do
        mkdir -p "$mountpoint" 2>/dev/null
        for dev in /dev/mmcblk0p1 /dev/mmcblk0; do
            [ -e "$dev" ] || continue
            log "mounting $dev on $mountpoint"
            mount -rw "$dev" "$mountpoint" 2>/dev/null && break 2
        done
    done
    [ -d "$BASE" ]
}

# Reads camera.conf. Every setting has a default here, so a missing or
# partial file still boots.
load_settings() {
    log_level=info
    metrics=
    ssh_user=root
    ssh_password=
    web_admin_user=admin
    web_admin_password=
    onvif_user=admin
    onvif_password=
    stop_vendor_ipc=1
    # Board profile name (boards/<name>.toml); empty = detect. An inherited
    # BOARD is kept unless camera.conf sets one.
    BOARD=${BOARD:-}
    # shellcheck source=/dev/null
    [ -f "$CAMERA_CONF" ] && . "$CAMERA_CONF"
    return 0
}

# Creates the sshd credentials file from camera.conf on first boot.
#
# The plaintext is removed from camera.conf once the hash is written, so it
# does not sit on a card that anyone can pull out and read. Only the
# PBKDF2 hash stays, in etc/.
provision_ssh() {
    mkdir -p "$BASE/etc" 2>/dev/null
    [ -s "$SSH_PASSWD" ] && return 0
    if [ -z "$ssh_password" ]; then
        log "WARNING: no $SSH_PASSWD and no ssh_password in camera.conf; sshd will not accept logins"
        return 1
    fi
    [ -x "$SSHD" ] || { log "WARNING: $SSHD missing, cannot hash the ssh password"; return 1; }

    log "hashing the ssh password for '$ssh_user' (takes a few seconds on this CPU)"
    # umask in a subshell so the rest of the boot keeps the default.
    if (umask 077 && printf '%s\n' "$ssh_password" | "$SSHD" hash "$ssh_user" > "$SSH_PASSWD.new" 2>/dev/null) &&
        [ -s "$SSH_PASSWD.new" ]; then
        mv -f "$SSH_PASSWD.new" "$SSH_PASSWD"
        # Blank the plaintext. BusyBox sed supports -i on these firmwares; if
        # it ever does not, say so rather than leave the password silently.
        if sed -i 's/^ssh_password=.*/ssh_password=/' "$CAMERA_CONF" 2>/dev/null; then
            log "ssh credentials written; plaintext removed from camera.conf"
        else
            log "WARNING: ssh credentials written but could not blank ssh_password in camera.conf"
        fi
        sync
    else
        rm -f "$SSH_PASSWD.new"
        log "ERROR: hashing the ssh password failed"
        return 1
    fi
    return 0
}

# Creates the web admin users file from camera.conf on first boot.
#
# Same rules as provision_ssh: only the PBKDF2 hash is kept, in etc/, and the
# plaintext is blanked in camera.conf. More users are added from the web UI
# later; this only seeds the first admin. To start over, delete etc/web-users
# and set web_admin_password again.
provision_web() {
    mkdir -p "$BASE/etc" 2>/dev/null
    [ -s "$WEB_USERS" ] && return 0
    if [ -z "$web_admin_password" ]; then
        log "WARNING: no $WEB_USERS and no web_admin_password in camera.conf; nobody can log in to the web UI"
        return 1
    fi
    [ -x "$WEB" ] || { log "WARNING: $WEB missing, cannot hash the web admin password"; return 1; }

    log "hashing the web admin password for '$web_admin_user' (takes a few seconds on this CPU)"
    # umask in a subshell so the rest of the boot keeps the default.
    if (umask 077 && printf '%s\n' "$web_admin_password" |
        "$WEB" hash "$web_admin_user" --role admin > "$WEB_USERS.new" 2>/dev/null) &&
        [ -s "$WEB_USERS.new" ]; then
        mv -f "$WEB_USERS.new" "$WEB_USERS"
        if sed -i 's/^web_admin_password=.*/web_admin_password=/' "$CAMERA_CONF" 2>/dev/null; then
            log "web admin credentials written; plaintext removed from camera.conf"
        else
            log "WARNING: web admin credentials written but could not blank web_admin_password in camera.conf"
        fi
        sync
    else
        rm -f "$WEB_USERS.new"
        log "ERROR: hashing the web admin password failed"
        return 1
    fi
    return 0
}

# Creates the ONVIF credentials file from camera.conf on first boot.
#
# Unlike the ssh and web passwords this one cannot be hashed: NVRs log in with
# WS-UsernameToken PasswordDigest, Base64(SHA1(nonce + created + password)),
# which the camera can only check if it holds the password itself. So the
# plaintext moves from camera.conf into etc/onvif.secret (mode 0600, one
# dedicated account, not a web user) rather than disappearing. It is still
# blanked in camera.conf, so it lives in one place only. To change it, delete
# etc/onvif.secret and set onvif_password again.
provision_onvif() {
    mkdir -p "$BASE/etc" 2>/dev/null
    [ -s "$ONVIF_SECRET" ] && return 0
    if [ -z "$onvif_password" ]; then
        log "WARNING: no $ONVIF_SECRET and no onvif_password in camera.conf; onvif will not start"
        return 1
    fi
    case "$onvif_user" in
        ''|*:*)
            log "ERROR: onvif_user must be non-empty and must not contain ':'"
            return 1
            ;;
    esac

    if (umask 077 && printf '%s:%s\n' "$onvif_user" "$onvif_password" > "$ONVIF_SECRET.new") &&
        [ -s "$ONVIF_SECRET.new" ]; then
        # FAT has no modes and ignores this; anywhere else onvif refuses
        # a secret other users can read.
        chmod 600 "$ONVIF_SECRET.new" 2>/dev/null
        mv -f "$ONVIF_SECRET.new" "$ONVIF_SECRET"
        if sed -i 's/^onvif_password=.*/onvif_password=/' "$CAMERA_CONF" 2>/dev/null; then
            log "ONVIF credentials written to $ONVIF_SECRET; removed from camera.conf"
        else
            log "WARNING: ONVIF credentials written but could not blank onvif_password in camera.conf"
        fi
        sync
    else
        rm -f "$ONVIF_SECRET.new"
        log "ERROR: writing $ONVIF_SECRET failed"
        return 1
    fi
    return 0
}

export_env() {
    # The board override from camera.conf, for the supervisor and (through
    # it) every service. Only when set: empty means detect.
    if [ -n "$BOARD" ]; then
        export BOARD
    fi

    # Every library comes from the binary's (and each SDK object's) own
    # $ORIGIN-relative RPATH: lib/<variant> for the SDK, lib/sys for the
    # runtime. An inherited LD_LIBRARY_PATH is dropped so nothing can slip a
    # firmware copy of an SDK object in between.
    unset LD_LIBRARY_PATH

    # Every Rust binary picks these up through telemetry, so each one
    # gets its own rotated log on the SD card.
    ANYKA_LOG=${ANYKA_LOG:-$log_level}
    ANYKA_LOG_FILE=${ANYKA_LOG_FILE:-$LOG_DIR/supervisor.log}
    ANYKA_LOG_MAX_BYTES=$LOG_MAX_BYTES
    ANYKA_LOG_KEEP=$LOG_KEEP
    export ANYKA_LOG ANYKA_LOG_FILE ANYKA_LOG_MAX_BYTES ANYKA_LOG_KEEP

    # Metrics are off unless camera.conf (or the caller) asks for them. With
    # node_exporter on the camera, the textfile collector is the cheap option;
    # the value names a directory and each service writes <service>.prom.
    ANYKA_METRICS=${ANYKA_METRICS:-$metrics}
    if [ -n "$ANYKA_METRICS" ]; then
        case "$ANYKA_METRICS" in
            textfile:*) mkdir -p "${ANYKA_METRICS#textfile:}" 2>/dev/null ;;
        esac
        export ANYKA_METRICS
    fi
}

# --------------------------------------------------------------------------
# Checks
# --------------------------------------------------------------------------

preflight() {
    ok=0

    if [ ! -x "$SUPERVISOR" ]; then
        log "ERROR: $SUPERVISOR is missing or not executable"
        ok=1
    fi
    if [ ! -f "$CONFIG" ]; then
        log "ERROR: $CONFIG is missing"
        ok=1
    fi
    check_runtime_libs || ok=1
    if [ "$ok" -eq 0 ]; then
        # The supervisor validates its own config and reports missing
        # executables, which is a better check than anything repeated here.
        if ! "$SUPERVISOR" --config "$CONFIG" --check; then
            log "ERROR: configuration check failed"
            ok=1
        fi
    fi
    return $ok
}

# --------------------------------------------------------------------------
# Board
# --------------------------------------------------------------------------

# Takes the vendor process list from the board profile, through
# `supervisor board --shell` (which prints single-quoted assignments of
# KAMODER_BOARD_NAME, VENDOR_PROCESSES and WATCHDOG_WAIT).
#
# Anything wrong — no binary, non-zero exit, empty or odd output — keeps the
# literal AV100 defaults above, which is exactly the behaviour from before
# board profiles. Only stdout is evaluated; the error, if any, goes to the
# log. Runs before the vendor IPC is touched, so it cannot affect the
# watchdog handover.
resolve_board() {
    board_out=
    if [ -x "$SUPERVISOR" ]; then
        board_out=$("$SUPERVISOR" board --shell) || board_out=
    fi
    # Tried in a subshell first: a syntax error in `eval` makes a
    # non-interactive POSIX shell exit, and this one must keep going.
    if [ -n "$board_out" ] && (eval "$board_out") >/dev/null 2>&1; then
        eval "$board_out"
    fi
    board_ok=1
    [ -n "${KAMODER_BOARD_NAME:-}" ] || board_ok=0
    case "${WATCHDOG_WAIT:-}" in
        0|1) ;;
        *) board_ok=0 ;;
    esac
    # Names are word-split and compared with /proc/<pid>/comm: refuse glob
    # or shell characters rather than let them expand (globbing is off
    # while checking, so the check itself expands nothing).
    set -f
    for name in ${VENDOR_PROCESSES:-}; do
        case "$name" in
            *[!A-Za-z0-9._+-]*) board_ok=0 ;;
        esac
    done
    set +f
    if [ "$board_ok" != 1 ]; then
        KAMODER_BOARD_NAME=
        VENDOR_PROCESSES=$DEFAULT_VENDOR_PROCESSES
        WATCHDOG_WAIT=1
        log "WARNING: board not resolved; using the built-in AV100 defaults (vendor processes: $VENDOR_PROCESSES)"
        return 1
    fi
    log "board $KAMODER_BOARD_NAME: vendor processes '$VENDOR_PROCESSES', watchdog_wait=$WATCHDOG_WAIT"
    return 0
}

# --------------------------------------------------------------------------
# Last-resort watchdog feeder
# --------------------------------------------------------------------------

# If the supervisor cannot run, the watchdog still has to be fed the moment
# anyone stops the vendor IPC process, or the board reboots in about eight
# seconds. This is the safety net, not the normal path: in the normal path the
# supervisor owns the feeder.
# Whether a process with this exact name is running.
#
# Scans /proc directly: `pidof` and `pkill` are both missing from some BusyBox
# builds on these cameras, and `ps | grep` needs an extra pipeline plus the
# usual dance to avoid matching the grep itself.
process_running() {
    name=$1
    for comm in /proc/[0-9]*/comm; do
        [ -r "$comm" ] || continue
        read -r running < "$comm" 2>/dev/null || continue
        [ "$running" = "$name" ] && return 0
    done
    return 1
}

start_fallback_feeder() {
    [ -x "$FALLBACK_FEEDER" ] || { log "no fallback feeder at $FALLBACK_FEEDER"; return 1; }
    process_running wdt_feeder && { log "watchdog feeder already running"; return 0; }
    log "starting fallback watchdog feeder"
    "$FALLBACK_FEEDER" -i 1 -w >> "$LOG_DIR/wdt_feeder.fallback.log" 2>&1 &
    return 0
}

# --------------------------------------------------------------------------
# Vendor IPC
# --------------------------------------------------------------------------

# Sends a signal to every process with this exact name. Returns 1 if there
# was none.
signal_process() {
    name=$1
    signal=$2
    found=1
    for comm in /proc/[0-9]*/comm; do
        [ -r "$comm" ] || continue
        read -r running < "$comm" 2>/dev/null || continue
        [ "$running" = "$name" ] || continue
        pid=${comm#/proc/}
        pid=${pid%/comm}
        kill "-$signal" "$pid" 2>/dev/null && found=0
    done
    return $found
}

# Stops the vendor IPC stack, as the old quiet-console script did, so the
# console goes quiet and the ISP, ADC and /dev/watchdog are free for the Rust
# stack.
#
# The watchdog is the dangerous part: `ipc` holds /dev/watchdog exclusively,
# so while it runs wdt_feeder cannot open it, and once it dies nothing feeds
# the timer — the board reboots about eight seconds later. That is why this
# runs only after preflight passed, waits until the processes are really gone
# (their exit releases the device), and is followed straight away by the
# supervisor, whose first service is the feeder.
stop_vendor_ipc() {
    if [ "$stop_vendor_ipc" != 1 ]; then
        log "stop_vendor_ipc=$stop_vendor_ipc; leaving the vendor IPC running"
        return 0
    fi
    for name in $VENDOR_PROCESSES; do
        signal_process "$name" KILL && log "stopped vendor process $name"
    done
    # SIGKILL cannot be ignored, but the exit is asynchronous. Wait for it so
    # the feeder does not find /dev/watchdog still busy.
    i=0
    while [ "$i" -lt 3 ]; do
        left=
        for name in $VENDOR_PROCESSES; do
            process_running "$name" && left="$left $name"
        done
        [ -z "$left" ] && return 0
        sleep 1
        i=$((i + 1))
    done
    log "WARNING: still running after SIGKILL:$left"
    return 1
}

# Keeps the vendor stack down for as long as the Rust stack runs.
#
# On the AV100 the firmware launches `anyka_ipc`/`main` well after the SD hook
# has run, so stop_vendor_ipc alone never sees them. By then wdt_feeder already
# holds /dev/watchdog, so killing them later is the safe order: they never got
# the device, and nothing stops feeding it. The guard only kills while
# wdt_feeder is running, so it can never be the reason the board reboots.
vendor_guard_loop() {
    while :; do
        if process_running wdt_feeder; then
            for name in $VENDOR_PROCESSES; do
                signal_process "$name" KILL && log "guard: stopped vendor process $name"
            done
        fi
        sleep "$GUARD_INTERVAL"
    done
}

start_vendor_guard() {
    [ "$stop_vendor_ipc" = 1 ] || return 0
    kill_pid_file "$GUARD_PID_FILE" TERM
    vendor_guard_loop &
    echo $! > "$GUARD_PID_FILE"
    log "vendor guard running (pid $!, every ${GUARD_INTERVAL}s)"
}

# --------------------------------------------------------------------------
# Supervisor lifecycle
# --------------------------------------------------------------------------

# Restart the supervisor if it ever exits.
#
# This loop blocks in `wait`; it is not a poll. The supervisor itself is
# event-driven, so on a healthy camera this wakes up exactly never.
run_supervisor_loop() {
    failures=0
    while :; do
        log "starting supervisor"
        "$SUPERVISOR" --config "$CONFIG" &
        child=$!
        echo "$child" > "$PID_FILE"
        wait "$child"
        status=$?

        # 0 means it was asked to stop; anything else is a crash.
        if [ "$status" -eq 0 ]; then
            log "supervisor exited cleanly, not restarting"
            break
        fi

        failures=$((failures + 1))
        if [ "$failures" -ge "$MAX_SUPERVISOR_RESTARTS" ]; then
            log "ERROR: supervisor failed $failures times, giving up"
            start_fallback_feeder
            break
        fi

        # Linear backoff, capped at 30 s. Keeps a broken binary from rewriting
        # the SD card in a tight loop.
        delay=$((failures * 3))
        [ "$delay" -gt 30 ] && delay=30
        log "supervisor exited with status $status, restarting in ${delay}s (attempt $failures)"
        sleep "$delay"
    done
    rm -f "$PID_FILE" "$LOOP_PID_FILE"
}

# Sends a signal to a pid recorded in a file, if it is still alive.
kill_pid_file() {
    file=$1
    signal=$2
    [ -f "$file" ] || return 1
    pid=$(cat "$file" 2>/dev/null)
    case "$pid" in
        ''|*[!0-9]*) return 1 ;;
    esac
    kill -0 "$pid" 2>/dev/null || return 1
    kill "-$signal" "$pid" 2>/dev/null
    return 0
}

stop_stack() {
    # Stop the restart loop first, so it does not spawn a replacement while
    # the supervisor is being shut down.
    #
    # Deliberately *not* `pkill -f`: a pattern broad enough to catch this
    # script is also broad enough to catch unrelated processes that merely
    # mention it, up to and including the shell running this function.
    if kill_pid_file "$LOOP_PID_FILE" TERM; then
        log "stopped the restart loop"
    fi
    rm -f "$LOOP_PID_FILE"
    # The guard next: with the supervisor (and so wdt_feeder) going away it
    # would stop killing anyway, but it should not outlive the stack.
    if kill_pid_file "$GUARD_PID_FILE" TERM; then
        log "stopped the vendor guard"
    fi
    rm -f "$GUARD_PID_FILE"

    if [ -f "$PID_FILE" ]; then
        pid=$(cat "$PID_FILE" 2>/dev/null)
        # SIGTERM: the supervisor stops its services in reverse order and
        # flushes the logs.
        if kill_pid_file "$PID_FILE" TERM; then
            log "stopping supervisor (pid $pid)"
            i=0
            while [ "$i" -lt 30 ] && kill -0 "$pid" 2>/dev/null; do
                sleep 1
                i=$((i + 1))
            done
            if kill -0 "$pid" 2>/dev/null; then
                log "supervisor did not stop, sending SIGKILL"
                kill -KILL "$pid" 2>/dev/null
            fi
        fi
        rm -f "$PID_FILE"
    fi
    log "stopped"
}

already_running() {
    [ -f "$PID_FILE" ] || return 1
    pid=$(cat "$PID_FILE" 2>/dev/null)
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null
}

# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------

main() {
    mode=${1:-background}

    if ! ensure_sd_mounted; then
        # No card, nothing to run from. Say so on the console and leave the
        # camera exactly as it was.
        echo "anyka_rust: $BASE not found, nothing to start" >&2
        return 0
    fi

    mkdir -p "$LOG_DIR" 2>/dev/null
    rotate "$INIT_LOG"
    load_settings

    case "$mode" in
        --stop)
            exec >> "$INIT_LOG" 2>&1
            stop_stack
            return 0
            ;;
        --check)
            export_env
            resolve_board
            preflight
            return $?
            ;;
        --fg) ;;
        background|*) ;;
    esac

    if already_running; then
        log "already running, nothing to do"
        return 0
    fi

    exec >> "$INIT_LOG" 2>&1
    log "----- anyka_rust starting from $BASE -----"

    export_env
    provision_ssh || log "continuing; the rest of the stack does not depend on sshd"
    provision_web || log "continuing; the rest of the stack does not depend on the web UI"
    provision_onvif || log "continuing; the rest of the stack does not depend on ONVIF"

    if ! preflight; then
        log "preflight failed; starting the fallback watchdog feeder only"
        start_fallback_feeder
        return 0
    fi

    # Before the vendor IPC is touched: the vendor app still feeds the
    # watchdog here, and a failure keeps the AV100 defaults.
    resolve_board || log "continuing with the built-in vendor process list"

    # Nothing may sit between this and starting the supervisor: from here the
    # watchdog is only fed once wdt_feeder is up.
    stop_vendor_ipc
    start_vendor_guard

    if [ "$mode" = "--fg" ]; then
        echo $$ > "$LOOP_PID_FILE"
        run_supervisor_loop
    else
        run_supervisor_loop &
        # Record the loop's pid from here: `$$` inside a backgrounded subshell
        # still reports the parent's pid in POSIX sh.
        echo $! > "$LOOP_PID_FILE"
    fi
    return 0
}

main "$@"
