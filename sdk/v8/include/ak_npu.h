#ifndef _AK_NPU_H_
#define _AK_NPU_H_

#include "ak_common.h"

enum ak_npu2_error_type {
    ERROR_NPU_ALREADY_INIT = ERROR_TYPE_NO_LEFT_SPACE + 2,
    ERROR_NPU_ALREADY_DEINIT,
};

/**
 * ak_npu_get_version: Get npu module version info
 * return: npu module version info
 */
const char* ak_npu_get_version(void);

/**
 * ak_npu_init - Init npu module
 * return: 0 success, other failed
 * notes:
 */
int ak_npu_init(void);

/**
 * ak_npu_deinit - Deinit npu module
 * return: 0 success, other failed
 * notes:
 */
int ak_npu_deinit(void);

/* ak_npu_reset_hw	-	reset SVP HW
 * return AK_SUCCESS is successful, error code otherwise.
 */
int ak_npu_reset_hw(void);

#endif
