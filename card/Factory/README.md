# Factory/

`config.sh` is the SD-card boot hook.

- **AK3918EV200:** the stock firmware runs `Factory/config.sh` by itself at boot
  (a factory-test check in `/usr/sbin/service.sh`). This is the trick
  [Gerge's Anyka hacking journey](https://gitea.raspiweb.com/Gerge/Anyka_ak3918_hacking_journey)
  found.
- **AK3918AV100:** the stock firmware runs nothing from the card. Start it by
  hand (`sh /mnt/disc1/Factory/config.sh`), or install the autostart patch
  (`tools/autostart/` in the repository).

`config.sh` starts the Rust stack from `anyka_rust/`, unless `camera.conf`
sets `run_rust=0`. If `anyka_rust/` is missing, it hands over to a legacy
`anyka_happy/` tree when there is one. Every call is logged to
`/tmp/anyka_config_sh.log`.
