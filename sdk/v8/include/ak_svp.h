#ifndef __AK_SVP__
#define __AK_SVP__


#include "ak_common.h"


enum ak_svp_error_type {
    ERROR_SVP_ALREADY_OPEN = ERROR_TYPE_NO_LEFT_SPACE + 2,
    ERROR_SVP_HANDLE_ID_ERROR,
    ERROR_SVP_HANDLE_ERROR,
    ERROR_SVP_NOT_CREATE,
    ERROR_SVP_SET_IOU_THRESHOLD,
    ERROR_SVP_SET_CLASSIFY_THRESHOLD,
    ERROR_SVP_SET_OBJECT_ERROR,
    ERROR_SVP_ALREADY_INIT,
    ERROR_SVP_ALREADY_DEINIT,
    ERROR_SVP_HANDLE_ID_NOT_CREATE,
    ERROR_TYPE_DMA_MALLOC_FAILED,
    ERROR_SVP_GET_DMA_FD_FAILED,
};

typedef struct AK_SVP_FEATURE_S
{
    unsigned int data_len;           //feature len;
    signed char *data;               //feature values,
}AK_SVP_FEATURE_T;

typedef enum
{
    AK_SVP_DAY_MODE = 0,        /* 白天模式 */
    AK_SVP_NIGHT_MODE           /* 夜间模式 */
}AK_SVP_ENV_MODE_E;

#define AK_SVP_NIDHT_MODE AK_SVP_NIGHT_MODE


typedef enum
{
    AK_SVP_HUMAN_SHAPE = 0,             /* human shape 人形 */
    AK_SVP_HUMAN_FACE,                  /* human face 人脸 */
    AK_SVP_HUMAN_SHAPE_AND_FACE,        /* human shape & face 人形&人脸 */
}AK_SVP_TARGET_TYPE_E;

typedef enum
{
    AK_SVP_MODEL_NORMAL = 0,            /* 正常模式 */
    AK_SVP_MODEL_HIGH_PERF,             /* 高性能模式 */
    AK_SVP_MODEL_RES_360P,              /* 640*360模式 */
    AK_SVP_MODEL_RES_360P_LOW_PERF,     /* 640*360 低性能模式 */
    AK_SVP_MODEL_FACE_EXTRACT = 5,      /* 人脸图片提取 */
    AK_SVP_MODEL_FACE_RECOGNIZE,        /* 人脸识别 */
    AK_SVP_MODEL_FAS                    /* 活体检测 */
}AK_SVP_MODEL_E;

typedef struct AK_SVP_THRESHOLD_S
{
    int classify_threshold;
    int IoU_threshold;
}AK_SVP_THRESHOLD_T;

typedef struct AK_SVP_CHN_ATTR_S
{
    AK_SVP_ENV_MODE_E       env_mode;         /*环境模式，AK3918AV100不支持*/
    AK_SVP_TARGET_TYPE_E    target_type;      /*目标检测类型*/
    AK_SVP_MODEL_E          model_type;       /*检测模型类型*/
    AK_SVP_THRESHOLD_T      threshold;
}AK_SVP_CHN_ATTR_T;

typedef int (*AK_SVP_CUS_READ_FUNC)(void *dst_buf,  int offset, int  size, const char  *path);

typedef enum
{
    AK_SVP_IMG_YUV420SP = 0,			/* YUV420SP，NV12*/
    AK_SVP_IMG_RGB_LI					/*RGB line-interlaced format */
}AK_SVP_IMG_TYPE_E;

typedef struct AK_SVP_POS_INFO_S
{
    int left;
    int top;
    int width;
    int height;
} AK_SVP_POS_INFO_T;

typedef struct AK_SVP_IMG_INFO_S
{
    AK_SVP_IMG_TYPE_E   img_type;
    int                 width;
    int                 height;
    AK_SVP_POS_INFO_T   pos_info;
    unsigned long       phy_addr;
    void                *vir_addr;
} AK_SVP_IMG_INFO_T;

typedef struct AK_SVP_RECT_S        //coordinate values, 坐标值
{
    AK_SVP_TARGET_TYPE_E    label;
    unsigned long           left;     //left_top point, horizontal axis, 左上角x值
    unsigned long           top;     //left_top point, vertical axis,   左上角y值
    unsigned long           right;     //right_bottom point, horizontal axis 右下角x值
    unsigned long           bottom;     //right_bottom point, vertical axis   右下角y值
    unsigned long           score; 		 // 识别出来的区域的得分
}AK_SVP_RECT_T;

typedef struct AK_SVP_OUTPUT_S
{
    int total_num;
    AK_SVP_RECT_T *target_boxes;		//coordinate values,坐标信息数组
}AK_SVP_OUTPUT_T;

typedef struct AK_SVP_DMA_BUFFER
{
    void          *pData;  // virtual address of the buffer
    unsigned long pa;      // physical address of the buffer
    unsigned int  size;    // size of the buffer
}AK_SVP_DMA_BUFFER_T;

/**
 * ak_svp_get_version - get version
 * return: version string
 */
const char* ak_svp_get_version(void);

/**
 * ak_svp_init - init svp. enable the hardware ready.
 * return: 0 - success; otherwise error code;
 */
int ak_svp_init(void);

/**
 * ak_svp_deinit - disable the hardware.
 * return: 0 - success; otherwise error code;
 */
int ak_svp_deinit(void);

/**
 * ak_svp_create_chn - setup the svp channel
 * @handle_id[IN]   :   the svp handle id
 * @attr[IN]        :   Chn attr to set
 * @pReadFunc[IN]   :   model parse func
 * @conf[IN]        :   model file
 * return AK_SUCCESS is successful,  error code otherwise.
 */
int ak_svp_create_chn(int handle_id, const AK_SVP_CHN_ATTR_T *attr, AK_SVP_CUS_READ_FUNC pReadFunc, const char *conf);

/**
 * ak_svp_create_chn_ext - create svp channel , need load model file in conf_dma_buf before call it
 * @handle_id[IN]   :   the svp handle id
 * @attr[IN]        :   Chn attr to set
 * @conf_dma_buf[IN]   :   model file load buf
 * return AK_SUCCESS is successful,  error code otherwise.
 * note: this API is only rtos supported
 */
int ak_svp_create_chn_ext(int handle_id, const AK_SVP_CHN_ATTR_T *attr , AK_SVP_DMA_BUFFER_T *conf_dma_buf);

/**
 * ak_svp_destroy_chn - close the svp channel
 * @handle_id[IN]   :   the svp handle id
 * return AK_SUCCESS is successful,  error code otherwise.
 */
int ak_svp_destroy_chn(int handle_id);

/**
 * ak_svp_process - start chn to work
 * @handle_id[IN]    :   the svp handle id
 * @input[IN]        :   input tensor
 * @output[OUT]       :   output tensor
 * return AK_SUCCESS is successful,  error code otherwise.
 */
int ak_svp_process(int handle_id,  AK_SVP_IMG_INFO_T *input,  AK_SVP_OUTPUT_T *output);

/**
 * ak_svp_release - release the output tensor
 * @handle_id[IN]    :   the svp handle id
 * @output[IN]       :   output tensor
 * return AK_SUCCESS is successful,  error code otherwise.
 */
int ak_svp_release(AK_SVP_OUTPUT_T *output);

/**
 * ak_svp_get_threshold - get threahold from the dest chn
 * @handle_id[IN]    :   the svp handle id
 * @threshold[OUT]    :   output tensor
 * return AK_SUCCESS is successful,  error code otherwise.
 */
int ak_svp_get_threshold(int handle_id, AK_SVP_THRESHOLD_T *threshold);

/**
 * ak_svp_set_threshold - set threahold to the dest chn
 * @handle_id[IN]    :   the svp handle id
 * @threshold[IN]    :   output tensor
 * return AK_SUCCESS is successful,  error code otherwise.
 */
int ak_svp_set_threshold(int handle_id, AK_SVP_THRESHOLD_T threshold);

/**
 * ak_svp_get_model - get the current working model info
 * @handle_id[IN]    :   the svp handle id
 * @model[OUT]       :   model info
 * return AK_SUCCESS is successful,  error code otherwise.
 */
int ak_svp_get_model(int handle_id, AK_SVP_MODEL_E *model);

/**
 * ak_svp_set_target_type - set the current working model output type
 * @handle_id[IN]    :   the svp handle id
 * @type[IN]         :   TYPE of output tensor
 * return AK_SUCCESS is successful,  error code otherwise.
 */
int ak_svp_set_target_type(int handle_id, AK_SVP_TARGET_TYPE_E type);

/**
 * ak_svp_feature_extract - start chn to do feature extract
 * @handle_id[IN]    :   the svp handle id
 * @input[IN]        :   input tensor
 * @output[OUT]       :   output tensor
 * return AK_SUCCESS is successful,  error code otherwise.
 */
int ak_svp_feature_extract(int handle_id,  AK_SVP_IMG_INFO_T *input,  AK_SVP_FEATURE_T *output);


/**
 * ak_svp_feature_release - release the output tensor
 * @output[IN]       :   output tensor
 * return AK_SUCCESS is successful,  error code otherwise.
 */
int ak_svp_feature_release(AK_SVP_FEATURE_T *output);

/* ak_svp_reset_hw	-	reset SVP HW
 * return AK_SUCCESS is successful, error code otherwise.
 */
int ak_svp_reset_hw(void);
#endif
