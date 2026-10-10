/*
 * Translation unit for the AK3918AV100 (V8) SDK.
 *
 * Deliberately narrower than the EV200 `wrapper.h`: the MVP covers video input
 * and the ISP motion statistics only. Widen it as modules are ported.
 *
 * These headers are self-contained — `ak_vi.h` typedefs `VI_DEV`/`VI_CHN`
 * itself and includes nothing — but `struct video_input_frame` embeds
 * `struct frame` from `ak_common.h`, so the order below matters.
 */
#include <stdio.h>
#include <signal.h>
#include <time.h>
#include <sys/types.h>

#include "ak_common.h"
#include "ak_common_video.h"
#include "ak_vi.h"
#include "ak_vpss.h"
