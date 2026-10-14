/*
 * Single translation unit handed to bindgen. Only the SDK modules that the
 * safe wrapper crate actually covers are listed; adding a header here is the
 * first step to exposing a new module.
 */
/* The SDK headers use `time_t`, `siginfo_t` and `FILE` without including the
 * standard headers that define them. */
#include <stdio.h>
#include <signal.h>
#include <time.h>
#include <sys/types.h>

#include "ak_common.h"
#include "ak_error.h"
#include "ak_global.h"
#include "list.h"

#include "ak_vi.h"
#include "ak_vpss.h"
#include "ak_venc.h"
#include "ak_rtsp.h"

#include "ak_ai.h"
#include "ak_ao.h"
#include "ak_aenc.h"

#include "ak_md.h"
#include "ak_osd.h"
#include "ak_drv_wdt.h"
#include "ak_drv_ir.h"
#include "ak_drv_irled.h"
#include "ak_drv_ptz.h"
#include "ak_thread.h"
