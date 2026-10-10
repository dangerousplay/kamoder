# /dev/preset_motor: userspace ABI (presetmotor.ko)

This ABI was worked out by static analysis only. Nothing was run on the camera.

- **Kernel module:** `ghidra/artifacts/modules/app_patch/presetmotor.ko` (md5 `e47b6da2…`). It is byte-identical to `mtd6_APP/rootfs/patch/ko/presetmotor.ko` and `tools/qemu-armv5/out/app.pristine/patch/ko/presetmotor.ko`. The module is not stripped. Addresses below are `.text` offsets inside the module.
- **Vendor app:** `ipc` (md5 `473cd02b…`, stripped). It is identical in `ghidra/artifacts/bins/ipc`, `mtd6_APP/rootfs/ipc` and `app.pristine/ipc`. Addresses below are ipc virtual addresses.
- **Tags:** **[C]** = CONFIRMED, meaning I read it in the disassembly and the address is cited. **[I]** = INFERRED.

> `references/rebuild/presetmotor/presetmotor.h` is **wrong** for this module. It uses a `'m'` magic, `_IOW` numbers and a `motor_parm` struct, and none of that matches the binary. Do not use it.

---

## 1. Module / device

| Item | Value | Tag |
|---|---|---|
| vermagic | `4.4.282 mod_unload ARMv5`. This is a **Linux 4.4** module, not 3.4. | [C] `.modinfo` |
| description / author | "motor driver with hrtimer Hisi3518Ev300" / "Digital Media Team ,Gwell crop" | [C] |
| Registration | `misc_register` with minor `0xff` (MISC_DYNAMIC_MINOR, so major 10 and a dynamic minor) and name `"preset_motor"`. udev/mdev creates `/dev/preset_motor`. | [C] `init_module`, `.data+0x0..0x8` |
| Module params | **None.** There is no `__param` section and no `parm=` in `.modinfo`. `insmod` takes no arguments. | [C] |
| Who loads it | `ipc` itself: `system("insmod /ipc/patch/ko/presetmotor.ko")` in fgPreSetMotorDevInit @0xeb5c8. `boot_openipc.sh` also loads it. | [C] |
| file_operations | Only `open`, `release` and `unlocked_ioctl`. There is **no** read, write, poll, mmap or compat_ioctl. | [C] fops at `.data+0x28`: `+0x20`→motor_ioctl, `+0x2c`→motor_open, `+0x34`→motor_release |
| init | printk. Then `misc_register`. Then `MotorGPIOFuncInit` requests 8 GPIOs and sets them all as outputs at 0. Then `hw_timer = ak_timer_probe(2)`. If the probe fails it prints "get hw timer for ptz fail", calls `vSetpMotorGPIO_exit`, frees the GPIOs and runs `misc_deregister`, **but still returns 0**. | [C] `init_module` |
| open | Sets `period_ms = max(period_ms, 2)`, then `ak_timer_start(ak_motor_hw_timer_handler, 0, hw_timer, 1000/period_ms)`. The default is 2 ms, i.e. 500 Hz. The timer runs from the first open. | [C] `motor_open` @0x24. The meaning of arg 4 (Hz) is [I]. |
| release | `ak_timer_stop(hw_timer)` **only**. The coil GPIOs are **not** switched off. If you close the fd in the middle of a move, the coils stay energised in their last phase. | [C] `motor_release` @0x0 |
| Timer tick | `vPreSetMotor_proc` @0x7e4 advances at most **one phase per axis per tick**. Both axes can move in the same tick. | [C] |
| State | One global state block in `.bss+8`. It is shared by every fd and there is no locking. | [C] |

### GPIOs and stepping (no limit switches)

| Axis (driver name) | Coil GPIOs A,B,C,D | Phase index var |
|---|---|---|
| H (`dir` 0/1, `H_*`, `left/right_border`) | 26, 55, 59, 57 | `.data+0x94` |
| V (`dir` 2/3, `V_*`, `up/down_border`) | 65, 66, 58, 69 | `.data+0xe8` |

- **[C]** The phase table at `.data+0x98` (4 entries × {A,B,C,D}) is `0011, 1001, 1100, 0110`. This is two-phase-on full stepping. The idle entry `.data+0xd8` is `0000`.
- **[C]** The module calls only `gpio_request`, `gpiod_direction_output_raw`, `gpiod_set_raw_value` and `gpio_free`. **There is no GPIO input, no IRQ and no limit switch or hall sensor.** Travel limits are purely software. Homing drives blind into the hard stop (see ioctl 7).

### Units

- **[C]** Every position or step value from userspace is **multiplied by 10** on input (`add r,r,r,lsl#2; lsl#1`) and **divided by 10** on output (`umull 0xCCCCCCCD, lsr#3` in vGetMotorNowPos @0x14d4).
- **[C]** Inside the driver, 1 unit = 1 phase step = 1 timer tick.
- **User unit ("step") = 10 phase steps.** Speed is `1000/(load_ms+1)` phase steps per second. With `motor_speed=4` that is 200 Hz, i.e. 50 ms per user step. [C] for the arithmetic; [I] that arg 4 is Hz.

---

## 2. ioctls

The command numbers are **plain integers 1..10**. They do **not** use `_IO/_IOR/_IOW` encoding: there is no magic, size or direction field. The dispatcher is `sub r3,cmd,#1; cmp r3,#9; ldrls pc,[pc,r3,lsl#2]` at motor_ioctl @0xd8.

Every call does a `printk("motor_drv: cmd=%d, parm=%lu")`, so dmesg gets spammed. Every command that takes input copies **exactly 40 bytes** (`arm_copy_from_user(..., 40)`). Every command that returns data copies **40 bytes** back, and all fields except the ones listed are zero.

If `access_ok` fails, the driver silently continues with an **all-zero struct**. The return value of `copy_to_user` is ignored. No command blocks; all of them return immediately.

| cmd | ipc name (log strings) | Dir | Reads (offsets) | Action | Returns | Handler |
|---|---|---|---|---|---|---|
| 1 | PRESET_MOTOR_BORDER_INIT | W | +0x00 H_border, +0x04 V_border | `dwMotorSetBorder(H*10, V*10)`: left=0, down=0, right=H*10, up=V*10. **now_pos is not changed.** | 0 | @0x194 → 0x1370 [C] |
| 2 | (none) | | | Invalid. Prints "Cmd Error" | -1 | @0x52c [C] |
| 3 | PRESET_MOTOR_SPEED | W | +0x08 load_ms | If `load_ms == 0` it returns -1. Otherwise it stops the timer, sets `period = load_ms+1` and restarts the timer at `1000/period`. | `ak_timer_start()` result (0 = ok) | @0x3c4 [C] |
| 4 | vPreSetMotorDevCtl ("run steps") | W | +0x0c preset_flag (u8), +0x0d dir (u8), +0x10 steps | `dwMotorRunSteps(flag, dir, steps*10)`. It always writes `flag` to the mode var. If dir is 0 or 1: `steps_H = s`, `direction_H = dir`. If dir is 2 or 3: `steps_V = s`, `direction_V = dir`. Any other dir does nothing on either axis. The move is relative. **steps = 0 is STOP for that axis.** | 0 | @0x2ac → 0x130c [C] |
| 5 | PRESET_MOTOR_RUN_POS | W | +0x0c preset_flag (u8), +0x14 H_preset_step, +0x18 V_preset_step | `dwMotorRunToPos(flag, H*10, V*10)`. It writes the mode var plus **both** `steps_H` and `steps_V`. With flag=1 the values are absolute targets. With flag=0 they are relative counts. | 0 | @0x324 → 0x134c [C] |
| 6 | "get now pos" | R | (none) | `vGetMotorNowPos`: now/10 | +0x1c H_now_pos, +0x20 V_now_pos; return 0 | @0x228 → 0x14d4 [C] |
| 7 | PRESET_MOTOR_POS_RESET | W | +0x00 H_border, +0x04 V_border | `vMotorPosRest(H*10, V*10)`: **homing**. See §2.2. | 0 | @0x478 → 0x1514 [C] |
| 8 | (debug) | | | `print_all_arg`: dumps the state to dmesg | 0 | @0x3bc [C] |
| 9 | "run status" | R | (none) | `run_status = running_H + running_V`, so the value is 0, 1 or 2 | +0x24 run_status; return 0 | @0x50c → 0x14ac [C] |
| 10 | "set now pos" | W | +0x1c H_now_pos, +0x20 V_now_pos | `now = H*10, V*10`. No clamp and no movement. | 0 | @0x110 → 0x1568 [C] |

The -1 return comes straight from the handler (`mvn r0,#0`). Userspace therefore sees `ioctl() == -1` with `errno == EPERM (1)` [C for -1; errno mapping is standard kernel behaviour].

### 2.1 Argument struct (40 bytes, the same struct for every command)

| Off | Size | Name (driver printk) | Used by |
|---|---|---|---|
| 0x00 | i32 | H_border (user steps) | 1, 7 |
| 0x04 | i32 | V_border | 1, 7 |
| 0x08 | i32 | load_ms (tick period − 1, in ms) | 3 |
| 0x0c | u8 | preSet_Flag (0 = relative, 1 = absolute target) | 4, 5 |
| 0x0d | u8 | direction (0/1 = H, 2/3 = V) | 4 |
| 0x0e | u16 | padding | |
| 0x10 | i32 | steps (relative, user steps) | 4 |
| 0x14 | i32 | H_preset_step (target) | 5 |
| 0x18 | i32 | V_preset_step | 5 |
| 0x1c | i32 | H_now_pos | 6 (out), 10 (in) |
| 0x20 | i32 | V_now_pos | 6 (out), 10 (in) |
| 0x24 | i32 | run_status | 9 (out) |

The struct is 40 bytes with `sizeof == 0x28`. Stack offsets in motor_ioctl use a base of `fp-0x3c` [C]. ipc callers build the same struct with the same offsets [C]:

- 0xeb858 writes steps at +0x10 and dir at +0x0d.
- 0xeaf88 writes the flag byte at +0x0c and H/V at +0x14/+0x18.
- 0xeb720 writes now_pos at +0x1c/+0x20.

### 2.2 Motion semantics (vPreSetMotor_proc @0x7e4)

Internal state lives at `S = .bss+8`. The field names come from `print_all_arg` @0x139c:

| Off | Field |
|---|---|
| +0x00 | reset_flag |
| +0x04 | H_reset_cnt |
| +0x08 | running_H |
| +0x0c | V_reset_cnt |
| +0x10 | running_V |
| +0x14 | preSet_Flag |
| +0x18 | steps_H |
| +0x1c | now_pos_H |
| +0x20 | right_border |
| +0x24 | steps_V |
| +0x28 | now_pos_V |
| +0x2c | up_border |
| +0x30 | direction_H |
| +0x34 | left_border |
| +0x38 | direction_V |
| +0x3c | down_border |

All comparisons are **unsigned**.

- **Homing (ioctl 7)** [C] 0x1514, 0x7e4–0x914, 0xb58–0xc3c:
  1. Sets now_H = now_V = 0, left = down = 0, right = H*10, up = V*10, `H_reset_cnt = H*10+40`, `V_reset_cnt = V*10+40`, and reset_flag = 1.
  2. On each tick, H steps in the "decreasing" direction (phase +1) and V steps in the "decreasing" direction (phase −1) until their counters reach 0. The driver does **no border check and no position update** during this, so it grinds into the mechanical stop for up to border+4 user steps.
  3. When both counters reach 0, reset_flag is cleared. The position is then (0,0) at the "left/down" end.
  4. While reset_flag is set, all other motion is deferred. It is not discarded: an ioctl 5 issued right after ioctl 7 runs after homing finishes, and ipc relies on this.
  5. Duration is about `(border*10+40) * (load_ms+1)` ms. With H=175 and load_ms=4 that is about 9 s.
- **Relative mode (preSet_Flag = 0)** [C] 0x9e0–0xc3c, 0xefc–0x1300. Each tick with `steps_X != 0` does `steps_X--` and then:

  | dir | Condition to step | Effect | At the border |
  |---|---|---|---|
  | 0 | now_H > left | now_H−− (phase +1) | That axis's coils go off and running = 0, but the counter keeps draining. |
  | 1 | now_H < right | now_H++ (phase −1) | Same |
  | 2 | now_V < up | now_V++ (phase +1) | Same |
  | 3 | now_V > down | now_V−− (phase −1) | Same |

  When both counters are 0, all 8 coils go off and running_H = running_V = 0 (0xc40–0xd20).
- **Absolute mode (preSet_Flag = 1)** [C] 0x918–0x9dc, 0xd24–0xef8. Each axis steps one tick toward its target.
  - When `target_H > right` or `target_V > up`, the handler **returns early for the entire tick**. Neither axis moves and the move never completes. **Clamp targets in userspace.**
  - Negative targets are huge unsigned values and hit the same stall.
  - When both axes have `now == target`, the driver sets steps_H = steps_V = 0 and preSet_Flag = 0. The next tick switches the coils off and sets running = 0.
- **run_status (ioctl 9)** is `running_H + running_V` [C] 0x14ac. **0 means idle, with coils released after the last tick.**
- **STOP** [C]:
  - `ioctl 4 {dir, steps=0}` stops one axis. This is what ipc does (see §3).
  - **Caveat:** ioctl 4 also forces preSet_Flag = the byte you pass (ipc passes 0). If an absolute move is in progress, the *other* axis's absolute target (×10) is then treated as a relative count and moves that far. The analysis is [C]; that it is a real hazard is [I].
  - The **safest stop for both axes is `ioctl 5 {flag=0, H=0, V=0}`**. It zeroes both counters in one call (0x134c stores all 3 fields).
  - Stop is an ioctl, not a position write. Writing now_pos (ioctl 10) does not stop anything.

---

## 3. How `ipc` drives it (config `support_preset_motor_drive=1`)

The `*Mcu*` functions (fgPreSetMcuMotorDevInit, vMcuMotorProc, …) are the `support_preset_mcu_motor_drive` path. That flag is 0 on this device, so they are **not used with /dev/preset_motor** [C: cfg idx 47 = 0; `motor_*` keys, not `mcu_motor_*`].

The config getter is `0x105c2c(idx)`, backed by a table at `0x5de258` with 8-byte entries {name, value} [C].

### device.config → driver fields [C]

| Key (idx) | Value here | Field |
|---|---|---|
| motor_bottom_steps (148) | 175 | +0x00 H_border (cmd 1, 7) |
| motor_top_steps (144) | 70 | +0x04 V_border |
| motor_speed (182) | 4 (0 → 5) | +0x08 load_ms (cmd 3) |
| motor_last_position (160) | 4949 = 0x1355 | H = (v & 0xff) × motor_last_position_bottom_multiple (162), which gives 85. V = ((v>>8) & 0xff) × motor_last_position_top_multiple (161), which gives 19. See 0xeb388 and the boot log "motor run to (85,19)". |
| motor_bottom_dir (146) / motor_top_dir (142) | 0 / 1 | Invert the H / V direction in vMotorRun (0xeaac8) |
| reset_sensor_direction (232) | 0 | XORs both dir flags |
| motor_bottom_scale (147) / motor_top_scale (143) | 27 / 16 | Duration factor for continuous moves (below) |

The H/V naming follows the driver strings. **"bottom" = H (pan), "top" = V (tilt).**

### Sequences

All calls go through `PlatformPreSetMOTOR_Ctl(cmd, &arg)` @0x260c0, which is `ioctl(fd, cmd, arg)` on an fd from `open("/dev/preset_motor", O_RDWR)` @0x26068. A pthread mutex is held around each call [C].

1. **fgPreSetMotorDevInit** @0xeb5c8 [C]:
   1. `insmod`, then `usleep`, then `open`.
   2. **cmd 3** {load_ms = motor_speed or 5}.
   3. **cmd 1** {H = 175, V = 70} ("preset motor border init").
   4. It then branches on runtime flags in the settings struct (`byte[5] bit0`, `byte[0x112] bit5`; meaning [I]):
      - Either **cmd 10** {now = saved last position}, which trusts the saved position with no homing.
      - Or **fgMotorPtzCheck**.
   5. The boot logs show a PtzCheck ("motor back to base point") running on every boot [C log].
2. **fgMotorPtzCheck** @0xeb434 and **perset_motor_reset** @0xeb8ec [C]:
   1. (perset_motor_reset only) cmd 3.
   2. **cmd 7** {H = 175, V = 70}, which homes the motor.
   3. **cmd 5** {flag = 1, H = 85, V = 19}, which goes to the last position. It is queued behind the homing.
   4. The vMotorProc ptzCheck branch (0xea410) then polls **cmd 6 once per second, up to 40 times**, until now == target ("ptzCheck timeout error!").
3. **Go to preset** (0xeaeb8, `fgMotorRunToPresetPosition`) [C]:
   1. **cmd 5** {flag = 1, H, V}.
   2. Poll **cmd 6 every 50 ms, up to 151 times** (about 7.5 s) until `H_now == H && V_now == V`.
   3. Saving a preset is **cmd 6**, which stores (H_now, V_now).
4. **Continuous move / "left, right, up, down"** [C]:
   1. The logical directions are 0 = left, 1 = right, 2 = up, 3 = down (set_motor_run strings). OnvifPTZMotorControl @0x12a904 calls `vMotorRun(2, dir)`.
   2. **vMotorRun** @0xeaac8 remaps the direction. H is swapped when `motor_bottom_dir ^ reset_sensor_direction ^ image_flip` is set; V is swapped when `motor_top_dir ^ reset_sensor_direction ^ image_flip` is set. The swap is 0↔1 or 2↔3.
   3. It then stores `duration_ms = n*((scale*10)/K) + n` and the direction into a global. K is `[0x5cbd84+0x14]`, a runtime value [I].
   4. The **vMotorProc** thread (0xea1c0) sees the request and issues **cmd 4** {flag = 0, dir, **steps = 3000**} (0xea7c8/0xea7e8). If the same direction is requested again, it only extends the deadline.
   5. When the deadline passes, it issues **cmd 4** {dir, **steps = 0**} (0xea5f4/0xea7a8). That is the stop. The motor borders stop it earlier if it reaches them.
   6. About 200 loop iterations after a move, it saves the last position with cmd 6 (0xeb228).
5. **Self-test sweep** (0x134348) [C]: `vMotorRun(175, 1)`, `vMotorRun(70, 2)`, `vMotorRun(175, 0)`, `vMotorRun(70, 3)`, with usleep between each, then it saves the position.

### Step ↔ degree

The **driver has no angle concept.** The travel range on this unit is **H 0..175 and V 0..70 user steps**, which is 1750 and 700 phase steps [C: config plus cmd 1/7].

The earlier notes' figures of "H 4096 steps/360°, V 1393/139°" come from a different firmware or log and **do not match this module's units** [I].

`motion_tracking_step_per_angle_x/y = 55/38` is used only by the floating-point motion-tracking code (0xef5f0…), and I did not decode what it means. **Calibrate degrees on hardware** by homing, running to 175 and measuring the angle [I].

---

## 4. Proposed Rust binding

```rust
#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct MotorArg {
    pub h_border: i32,       // 0x00
    pub v_border: i32,       // 0x04
    pub load_ms: i32,        // 0x08  tick period = load_ms+1 ms (must be >0)
    pub preset_flag: u8,     // 0x0c  0=relative, 1=absolute
    pub direction: u8,       // 0x0d  0=H-,1=H+,2=V+,3=V-
    pub _pad: u16,           // 0x0e
    pub steps: i32,          // 0x10
    pub h_preset_step: i32,  // 0x14
    pub v_preset_step: i32,  // 0x18
    pub h_now_pos: i32,      // 0x1c
    pub v_now_pos: i32,      // 0x20
    pub run_status: i32,     // 0x24  running_H + running_V
}
const _: () = assert!(core::mem::size_of::<MotorArg>() == 40);

// Raw ("bad") request numbers — NOT _IOC-encoded.
nix::ioctl_write_ptr_bad!(pm_set_border,  1,  MotorArg);
nix::ioctl_write_ptr_bad!(pm_set_speed,   3,  MotorArg);
nix::ioctl_write_ptr_bad!(pm_run_steps,   4,  MotorArg);
nix::ioctl_write_ptr_bad!(pm_run_to_pos,  5,  MotorArg);
nix::ioctl_read_bad!     (pm_get_pos,     6,  MotorArg); // kernel writes all 40 bytes
nix::ioctl_write_ptr_bad!(pm_home,        7,  MotorArg);
nix::ioctl_none_bad!     (pm_dump_dmesg,  8);
nix::ioctl_read_bad!     (pm_get_status,  9,  MotorArg);
nix::ioctl_write_ptr_bad!(pm_set_now_pos, 10, MotorArg);
```

### Driver rules

- Open with O_RDWR and keep the fd open for the life of the daemon. Close only when `run_status == 0` (see release above).
- **Run only one owner.** Kill or disable `ipc` motor handling first; the driver state is global and unlocked.
- On start, issue cmd 3 (load_ms = 4), then cmd 1 (175, 70). Then either cmd 10 with the persisted position, or cmd 7 to home (about 9 s; poll cmd 9 until 0).
- Clamp targets to `0..=border` before cmd 5.
- For "stop all", use cmd 5 {flag = 0, 0, 0}. For a jog, use cmd 4 {dir, n} followed by cmd 4 {dir, 0}.
- Poll cmd 9 and cmd 6 at no more than about 20 Hz, because every ioctl printks.

---

## 5. Notes added while implementing `anyka-ptz` (T8)

- **Where `device.config` lives.** On this firmware it is `/rom/device.config`. `auto_upgrade.sh` and `button_upgrade.sh` copy `custom_setting.ini` there, and the dumped copy (`firmware_dump/.../mtd7_ROM/rootfs/device.config`) holds the `motor_*` values listed in §3 [C]. It uses plain `key=value` lines with `#` comments. Nothing here shows that `ipc` reads that path rather than a copy [I].
- **`run_status` right after a motion ioctl.** The `running_*` flags are set only by the timer tick, so a cmd 9 issued before the next tick can still read 0 for a move that has just started [I, from §2.2]. The backend waits at least `max(poll_interval, 3 ticks)` before its first poll.
- **`run_status` during homing.** Unknown. §2.2 does not say whether homing sets `running_*` [I]. The backend therefore does not poll during homing. It waits the computed `(max(border)*10+40)*(load_ms+1)` ms first, then polls cmd 9 until it reads 0.
- **Diagonal moves.** Two cmd 4 calls work: one for dir 0/1 and one for dir 2/3. The tick advances both axes, and cmd 4 only touches the counter and direction of the axis you name (§1 "Timer tick", §2 cmd 4) [C].
