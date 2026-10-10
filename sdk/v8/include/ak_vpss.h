#ifndef __AK_VPSS_H__
#define __AK_VPSS_H__


#define VPSS_MD_DIMENSION_H_MAX     32
#define VPSS_MD_DIMENSION_V_MAX     24

#define VPSS_OD_AF_STATICS_MAX      5
#define VPSS_OD_RGB_HIST_MAX        256

#define VPSS_MASK_AREA_MAX	        4

#define NIGHT_ARRAY_NUM		        5
#define DAY_ARRAY_NUM		        10

enum mode_state {
    STATE_DAY,
    STATE_NIGHT,
};


enum vpss_mask_color_type {
    VPSS_MASK_ORIGINAL_COLOR = 0,	/* masked with original color*/
    VPSS_MASK_MOSAIC_VIDEO,			/* masked with mosaic video data*/
};


enum vpss_effect_type {
    /* HUE to SHARP, value: [-50, 50], 0 means use the value in ISP config file */
    VPSS_EFFECT_HUE = 0x00,
    VPSS_EFFECT_BRIGHTNESS,
    VPSS_EFFECT_SATURATION,
    VPSS_EFFECT_CONTRAST,
    VPSS_EFFECT_SHARP,
    VPSS_EFFECT_WDR,

    VPSS_STYLE_ID,		//[0, 2]
    VPSS_POWER_HZ,		//50 or 60
    VPSS_FLIP_MIRROR    //[0, 3]
};

enum vpss_gain_stat {
    VPSS_GAIN_NO_CHANGE_STAT = 0,
    VPSS_GAIN_LOW_STAT,
    VPSS_GAIN_HIGH_STAT,
    VPSS_GAIN_MID_STAT,
};

enum vpss_isp_op_type{
    VPSS_OP_TYPE_MANUAL = 0,
    VPSS_OP_TYPE_AUTO = 1,
    VPSS_OP_TYPE_BUTT,
};

struct vpss_md_info {
    unsigned short stat[VPSS_MD_DIMENSION_V_MAX][VPSS_MD_DIMENSION_H_MAX];
};

struct vpss_od_info {
    unsigned int af_statics[VPSS_OD_AF_STATICS_MAX];
    unsigned int rgb_total;
    unsigned int rgb_hist[VPSS_OD_RGB_HIST_MAX];
};

struct vpss_af_stat_info {
   unsigned int  af_statics[5];
};

struct vpss_af_attr {
    unsigned short  af_win0_left; 	//[0, 4095]
    unsigned short  af_win0_right;	//[0, 4095]
    unsigned short  af_win0_top;	    //[0, 4095]
    unsigned short  af_win0_bottom;   //[0, 4095]

    unsigned short  af_win1_left; 	//[0, 4095]
    unsigned short  af_win1_right;	//[0, 4095]
    unsigned short  af_win1_top;	    //[[0, 4095]
    unsigned short  af_win1_bottom;   //[0, 4095]

    unsigned short  af_win2_left; 	//[0, 4095]
    unsigned short  af_win2_right;	//[0, 4095]
    unsigned short  af_win2_top;	    //[0, 4095]
    unsigned short  af_win2_bottom;   //[0, 4095]

    unsigned short  af_win3_left; 	//[0, 4095]
    unsigned short  af_win3_right;	//[0, 4095]
    unsigned short  af_win3_top;	    //[0, 4095]
    unsigned short  af_win3_bottom;   //[0, 4095]

    unsigned short  af_win4_left; 	//[0, 4095]
    unsigned short  af_win4_right;	//[0, 4095]
    unsigned short  af_win4_top;	    //[0, 4095]
    unsigned short  af_win4_bottom;   //[0, 4095]

    unsigned short   af_th;       //[0, 128]

};

struct vpss_mask_masic_attr{
    unsigned short	mosai_size_hor;
    unsigned short	mosai_size_vec;
};

struct vpss_mask_color_info {
    /* defined by enum vpss_mask_color_type*/
    unsigned char color_type;

    /* range [0, 0xff]. if 0xff mask video opaquely, then can not see image */
    unsigned char mk_alpha;

    /* range [0, 0xff] */
    unsigned char r_mk_color;
    /* range [0, 0xff] */
    unsigned char g_mk_color;
    /* range [0, 0xff] */
    unsigned char b_mk_color;

    struct vpss_mask_masic_attr masic_attr;
};

struct vpss_mask_area {
    unsigned short start_xpos;
    unsigned short end_xpos;
    unsigned short start_ypos;
    unsigned short end_ypos;
    unsigned char enable;
};

struct vpss_mask_area_info {
    struct vpss_mask_area main_mask[VPSS_MASK_AREA_MAX];
};

struct vpss_isp_ae_run_info {
    unsigned char   current_calc_avg_lumi;         		//现在的计算出的亮度值
    unsigned char   current_calc_avg_compensation_lumi;	//经过曝光补偿后的亮度值

    unsigned char	current_target_lumi;
    unsigned char	is_stable;

    int  current_a_gain;				//模拟增益的值
    int  current_d_gain;				//数字增益的值
    int  current_isp_d_gain;			//isp数字增益的值
    int  current_exp_time;				//曝光时间的值

    unsigned int  current_a_gain_step;	//现在的模拟增益的步长
    unsigned int  current_d_gain_step; //数字增益的步长
    unsigned int  current_isp_d_gain_step;	//isp数字增益的步长
    unsigned int  current_exp_time_step;	//曝光时间的步长
};


struct vpss_isp_ae_init_info {
    long  a_gain;				//模拟增益的值
    long  d_gain;				//数字增益的值
    long  isp_d_gain;			//isp数字增益的值
    long  exp_time;				//曝光时间的值
};

enum vpss_isp_win_weight_type {
   VPSS_AE_CENTER_WGHT= 0,
   VPSS_AE_AVERAGE = 1,
   VPSS_AE_SPOT ,
   VPSS_AE_WGHT_END ,
};

struct vpss_isp_weight {
   unsigned short	zone_weight[8][16];
};

struct vpss_isp_weight_attr {
    enum vpss_isp_win_weight_type   ae_win_weight_type;
    struct vpss_isp_weight      	win_weight[4];
};

struct vpss_isp_ex_zweight_attr {
    unsigned short	win_weight[8][16];
    unsigned short	ex_zone_weight_str;
};

struct vpss_isp_ae_ex_convergence_attr {
    unsigned int   ae_ex_convergence_en;
    unsigned int   control_zone1;
    unsigned int   control_zone2;
    unsigned int   control_zone3;
    unsigned int   control_zone4;
    unsigned int   control_step1;
    unsigned int   control_step2;
    unsigned int   control_step3;
    unsigned int   control_step4;
    unsigned int   control_step_max;
};

enum vpss_isp_ae_strgy_model {
    VPSS_AE_EXP_HIGHLIGHT_PRIOR = 0,
    VPSS_AE_EXP_LOWLIGHT_PRIOR,
    VPSS_AE_STRGY_MODEL_BUFF,
};

struct vpss_isp_ae_strgy_attr {
    unsigned int	hist_weight[16];
};

struct vpss_isp_ae_route_node_attr {
    unsigned int   exp_time;
    unsigned int   again;
    unsigned int   dgain;
    unsigned int   isp_dgain;
};

struct vpss_isp_ae_route_attr{
    unsigned int   ae_route_en;
    unsigned int   ae_route_node_num;
    struct vpss_isp_ae_route_node_attr  ae_route_node[16];
};


struct vpss_isp_ae_attr {
    unsigned int    exp_time_max;			//曝光时间的最大值
    unsigned int    exp_time_min;			//曝光时间的最小值
    unsigned int    d_gain_max;      		//数字增益的最大值
    unsigned int    d_gain_min;     		//数字增益的最小值
    unsigned int    isp_d_gain_min;  		//isp数字增益的最小
    unsigned int    isp_d_gain_max;  		//isp数字增益的最大值
    unsigned int    a_gain_max;     		//模拟增益的最大值
    unsigned int    a_gain_min;      		//模拟增益的最小值
    unsigned int    exp_step;            	//用户曝光调整步长
    unsigned int    exp_stable_range;     //稳定范围
    unsigned int    exp_hold_range;
    unsigned int    exp_speed;
    unsigned int    anti_flicker_target_lumi;
    unsigned int    anti_flicker_start_exp;//[1,100]
    unsigned int    exp_lumi_filter_para;
    unsigned int    target_lumiance[16]; 		//目标亮度
    unsigned int    envi_gain_range[16][2];
   // unsigned int	 hist_weight[16];
    unsigned int    blacklight_compensation_en;
    unsigned int    blacklight_detect_scope;
    unsigned int    blacklight_rate_max;
    unsigned int    blacklight_rate_min;
    enum vpss_isp_win_weight_type           ae_win_weight_type;
    struct vpss_isp_weight                  win_weight[4];
    struct vpss_isp_ae_ex_convergence_attr  ae_ex_convergence_para;
    enum vpss_isp_ae_strgy_model            ae_strgy_type;
    struct vpss_isp_ae_strgy_attr           ae_strgy_para[3];
    struct vpss_isp_ae_route_attr           exp_route_para;
};

struct vpss_isp_me_attr {
    unsigned int exp_time;
    unsigned int a_gain;
    unsigned int d_gain;
    unsigned int isp_d_gain;
};

struct vpss_isp_awb_block_stat_info{
    unsigned int r_awb_block_stat[24][32];
    unsigned int g_awb_block_stat[24][32];
    unsigned int b_awb_block_stat[24][32];
};

struct vpss_isp_awb_run_info{
    unsigned short	 r_gain;
    unsigned short	 g_gain;
    unsigned short	 b_gain;
    signed short   r_offset;
    signed short   g_offset;
    signed short   b_offset;
    unsigned char    is_stable;
    unsigned short	 current_colortemp_index;
    unsigned short	 colortemp_stable_cnt[10];
    unsigned short current_colortemp;
};


struct vpss_isp_awb_stat_info {
    //在白平衡统计参数范围内的白平衡统计结果
    unsigned long  total_R[10];	//10个色温每一个色温下的R分量像素值
    unsigned long  total_G[10]; //10个色温每一个色温下的G分量像素值
    unsigned long  total_B[10]; //10个色温每一个色温下的B分量像素值
    unsigned long  total_cnt[10];	//在白平衡统计参数范围内的像素数量值

    struct vpss_isp_awb_block_stat_info	wb_block_stat_info;
    struct vpss_isp_awb_run_info run_info;
};

struct ak_auto_day_night_threshold {
    int day_to_night_lum;	// day to night lum value
    int night_to_day_lum;	// night to day lum value
    int night_cnt[NIGHT_ARRAY_NUM];	// awb night cnt array
    int day_cnt[DAY_ARRAY_NUM];	// awb day cnt array
    int lock_time;					// locke night status time
    int quick_switch_mode;			// quick switch mode
    int day2night_sleep_time;       // day change to night sleep time
    int night2day_sleep_time;       // night change to day sleep time
};

struct ak_soft_ps_attr {
    unsigned int    day_to_night_lum;	// day to night lum value
    unsigned int    night_to_day_lum;	// night to day lum value
    unsigned int    awb_night_dis_min;
    unsigned int    awb_night_dis_max;
    unsigned int    day_check_frame_num;
    unsigned int    night_check_frame_num;
    unsigned int    exp_stable_check_frame_num;
    unsigned int    stable_wait_timeout_ms;
    unsigned int    gain_r;
    int             offset_r;
    unsigned int    gain_b;
    int             offset_b;
    unsigned int    gain_rb;
    int             offset_rb;
    unsigned int    blk_check_flag;
    unsigned int    low_lumi_th;
    unsigned int    high_lumi_th;
    unsigned int    non_infrared_ratio_th;
    unsigned int    highlight_lumi_th;
    unsigned int    highlight_blk_cnt_max;
    unsigned int    lock_cnt_th;
    unsigned int    lock_time_ms;
    unsigned int    revised_dis_max_ratio;
};

struct ak_non_infrared_ratio_attr {
    unsigned int    awb_night_dis_max;
    unsigned int    gain_r;
    int             offset_r;
    unsigned int    gain_b;
    int             offset_b;
    unsigned int    gain_rb;
    int             offset_rb;
    unsigned int    low_lumi_th;
    unsigned int    high_lumi_th;
    unsigned int    non_infrared_ratio_th;
    unsigned int    revised_dis_max_ratio;
};

struct vpss_isp_wdr {
    unsigned short   hdr_uv_adjust_level;
    unsigned short   hdr_cnoise_suppress_slop;
    unsigned short   wdr_enable;

    unsigned short	wdr_th1;	  //0-255
    unsigned short	wdr_th2;	  //0-255
    unsigned short	wdr_th3;	  //0-255
    unsigned short	wdr_th4;	  //0-255
    unsigned short	wdr_th5;	  //0-255

    //unsigned short wdr_light_weight;

    unsigned short	area_tb1[65];	  //10bit
    unsigned short	area_tb2[65];	  //10bit
    unsigned short	area_tb3[65];	  //10bit
    unsigned short	area_tb4[65];	  //10bit
    unsigned short	area_tb5[65];	  //10bit
    unsigned short	area_tb6[65];	  //10bit

    unsigned short	area1_key[16];
    unsigned short	area2_key[16];
    unsigned short	area3_key[16];
    unsigned short	area4_key[16];
    unsigned short	area5_key[16];
    unsigned short	area6_key[16];

    unsigned short   hdr_uv_adjust_enable;
    unsigned short   hdr_cnoise_suppress_yth1;
    unsigned short   hdr_cnoise_suppress_yth2;
    unsigned short   hdr_cnoise_suppress_gain;
};

struct vpss_wdr_attr {
    enum vpss_isp_op_type wdr_mode;
    struct vpss_isp_wdr manual_wdr;
    struct vpss_isp_wdr linkage_wdr[16];
};

struct vpss_isp_wb_type_attr {
    enum vpss_isp_op_type wb_type;
};

struct vpss_isp_3d_nr {
    unsigned short		tnr_enable; 		//default:1(GUI edit)
    unsigned short		update_ref;
    unsigned short		tnr_refFrame_format;

    unsigned short		t_mf_th1;	//[0, 8191],default:300(GUI edit)
    unsigned short		t_mf_th2;	//[0, 8191],default:500(GUI edit)
    unsigned short		t_ex_mf;
    //unsigned short		t_mf_slop; //[0,255] //65536/(t_mf_th2-t_mf_th1)
    unsigned short		statScale[129]; //[0, 15],default:8(GUI edit)
    unsigned short		statScale_key[16];

    unsigned short		ynr_k;			//[0,15]
    unsigned short		ynr_calc_k;		//[0,65535](GUI edit)
    unsigned short		ynr_weight_tbl[17];//ynr_strength(GUI edit)
    unsigned short		ylp_k1; 		//[0, 15],default:7(GUI edit)
    unsigned short		ylp_k2; 		//[0, 15],default:0(GUI edit)
    unsigned short		t_y_th1;		//[0, 511],default:48(GUI edit)
    unsigned short		t_y_th2;		//[0, 511],default:16(GUI edit)
    unsigned short		t_y_low;		//[0,127]
    unsigned short		t_y_low2;		//[0,127]
    unsigned short		t_y_k1; 		//[0, 127],default:120(GUI edit)
    unsigned short		t_y_k2; 		//[0, 127],default:120(GUI edit)
    unsigned short		t_y_ex_k;		//[0, 15]
    unsigned short		t_y_kslop;		//[0, 127],default:32(GUI edit)
    unsigned short		t_y_kslop2; 	//[0, 127]
    unsigned short		y_minstep;	//[0-15],default:2
    unsigned short		t_y_src_choose_k_th;		//[0, 127]
    unsigned short		t_y_uvassist;


    unsigned short		uvnr_k; 		//[0, 15],default:8(GUI edit)
    unsigned short		uvlp_k1;		//[0, 15],default:7(GUI edit)
    unsigned short		uvlp_k2;		//[0, 15],default:0(GUI edit)
    unsigned short		t_uv_th1;		//[0, 511],default:48(GUI edit)
    unsigned short		t_uv_th2;		//[0, 511],default:16(GUI edit)
    unsigned short		t_uv_diff_scale;//[0, 15]
    unsigned short		t_uv_k_low; 	//[0, 127]
    unsigned short		t_uv_k_low2;	//[0, 127]
    unsigned short		t_uv_k1;		//[0, 127],default:120(GUI edit)
    unsigned short		t_uv_k2;		//[0, 127]
    unsigned short		t_uv_ex_k;		//[0, 15],
    unsigned short		t_uv_kslop; 		//[0, 127],default:32(GUI edit)
    unsigned short		t_uv_kslop2;		//[0, 127],default:32(GUI edit)
    unsigned short		t_uv_minstep;		//[0, 15],
    unsigned short		t_uv_src_choose_k_th; //[0, 127]

    unsigned short		sharp_k_th; 			//[0, 127]
    unsigned short		sharp_factor_slop;		//[0, 15]
    unsigned short		sharp_factor_max;		//[0, 15]
    unsigned short		motion_filter;			//[0, 15]
    unsigned int		md_th;			//[0, 65535]
    unsigned short 		tnr_debug_output;	//[0,1]
    unsigned short      tnr_ref_statistic_en;
};

struct vpss_isp_3d_nr_attr {
    enum vpss_isp_op_type  _3d_nr_mode;
    struct vpss_isp_3d_nr manual_3d_nr;
    struct vpss_isp_3d_nr linkage_3d_nr[16];
};

struct vpss_isp_mwb_attr {
    unsigned short r_gain;
    unsigned short g_gain;
    unsigned short b_gain;
    short r_offset;
    short g_offset;
    short b_offset;
};

enum vpss_isp_awb_algo_type{
    VPSS_AWB_ALGO_DEFAULT = 0,
    VPSS_AWB_ALGO_AVERAGE_WEIGHT = 1,
    VPSS_AWB_ALGO_BLOCK_ASSIST,
    VPSS_AWB_ALGO_GW,
    VPSS_AWB_ALGO_BUTT,
};

typedef struct vpss_isp_awb_stat_calib_para{
    unsigned short	 gr_low[10];			//gr_low[i]<=gr_high[i]
    unsigned short	 gb_low[10];			//gb_low[i]<=gb_high[i]
    unsigned short	 gr_high[10];
    unsigned short	 gb_high[10];
    unsigned short	 rb_low[10];		   //rb_low[i]<=rb_high[i]
    unsigned short	 rb_high[10];
}VPSS_ISP_AWB_STAT_CALIB_PARA;


struct vpss_isp_awb_attr {
    unsigned short g_weight[16];
    unsigned short y_low;				//y_low<=y_high
    unsigned short y_high;
    unsigned short err_est;
    unsigned short 	 awb_iso_track_enable;
    VPSS_ISP_AWB_STAT_CALIB_PARA awb_stat_calib_para[16];
    #if 0
    unsigned short gr_low[10];			//gr_low[i]<=gr_high[i]
    unsigned short gb_low[10]; 			//gb_low[i]<=gb_high[i]
    unsigned short gr_high[10];
    unsigned short gb_high[10];
    unsigned short rb_low[10];			//rb_low[i]<=rb_high[i]
    unsigned short rb_high[10];
    #endif
    //awb软件部分需要设置的参数
    unsigned short auto_wb_step;                //白平衡步长计算
    unsigned short total_cnt_thresh;            //像素个数阈值
    unsigned short colortemp_stable_cnt_thresh; //稳定帧数，多少帧一样认为环境色温改变
    unsigned short colortemp_envi[10];
    enum vpss_isp_awb_algo_type alg_type;
};

struct vpss_isp_exp_type {
    enum vpss_isp_op_type   exp_type;
};

struct vpss_isp_sensor_reg_info {
    unsigned short reg_addr;
    unsigned short value;
};

struct vpss_lens_coef{
    unsigned short coef_b[10];    //[0,255]
    unsigned short coef_c[10];    //[0,1023]
};

struct vpss_lsc_attr {
    unsigned short          enable;
    //the reference point of lens correction
    unsigned short          xref;        //[0,4096]
    unsigned short          yref;        //[0,4096]
    unsigned short          lsc_shift;   //[0,15]
    struct vpss_lens_coef   lsc_r_coef;
    struct vpss_lens_coef   lsc_gr_coef;
    struct vpss_lens_coef   lsc_gb_coef;
    struct vpss_lens_coef   lsc_b_coef;
    //the range of ten segment
    unsigned short          range[10];   //[0,1023]
    unsigned short          lsc_mode;
    unsigned char           linkage_strength[17];   //[0,100]
};

struct vpss_fps_level {
    int high_fps;
    int high_fps_exp_time;
    int high_fps_to_lower_fps_gain;

    int mid_fps;                    //set 0 means no use middle fps
    int mid_fps_exp_time;           //set 0 means no use middle fps
    int mid_fps_to_low_fps_gain;    //set 0 means no use middle fps

    int low_fps;
    int low_fps_exp_time;
    int low_fps_to_higher_fps_gain;
};

struct vpss_yuv_effect_attr {
    unsigned short	y_a;	 // [0, 255]
    signed short    y_b;	   //[-128, 127]
    signed short    uv_a;    //[-256, 255]
    signed short    uv_b;    //[-256, 255]
    unsigned short	dark_margin_en;
};

struct vpss_rgb_gamma {
    unsigned short	 r_gamma[129];	 //10bit
    unsigned short	 g_gamma[129];	 //10bit
    unsigned short	 b_gamma [129];  //10bit
    unsigned short	 r_key[16];
    unsigned short	 g_key[16];
    unsigned short	 b_key[16];
    unsigned short	 rgb_gamma_enable;
};

struct vpss_rgb_gamma_attr {
    unsigned short	        gain_threshold;
    struct vpss_rgb_gamma   linkage_rgb_gamma[2];
};

struct vpss_contrast {
     unsigned short     y_contrast;	//[0,511]
     signed short       y_shift; 	//[0, 511]
};

struct vpss_auto_contrast {
    unsigned short	dark_pixel_area;	//[0, 511]
    unsigned short	dark_pixel_rate;	//[1, 256]
    unsigned short	shift_max;			//[0, 127]
};

struct vpss_contrast_attr {
    enum vpss_isp_op_type       cc_mode;
    struct vpss_contrast        manual_contrast;
    struct vpss_auto_contrast	linkage_contrast[16];
};

struct vpss_lce {
    unsigned short	  lce_enable;
    unsigned short	  lce_uv_adjust_en;
    unsigned short	  lce_strength[4][8];
    unsigned short	  lce_hist_weight[8];
    unsigned short	  lce_uv_adjust_level;
    unsigned short	  lce_weight_k;
};

struct vpss_lce_attr {
    enum vpss_isp_op_type	lce_mode;
    struct vpss_lce		    manual_lce;
    struct vpss_lce		    linkage_lce[16];
};

struct vpss_y_gamma_attr {
    unsigned short	  ygamma[129];	  //10bit
    unsigned short	  ygamma_key[16];
    unsigned short	  ygamma_uv_adjust_enable;
    unsigned short	  ygamma_uv_adjust_level;
    unsigned short	  ygamma_cnoise_yth1;
    unsigned short	  ygamma_cnoise_yth2;
    unsigned short	  ygamma_cnoise_slop;
    unsigned short	  ygamma_cnoise_gain ;
};

struct sharp_reduce_factor_attr {
    unsigned short sharp_reduce_factor[24][32];
};

struct vpss_sharp_reduce_factor_attr {
    struct sharp_reduce_factor_attr manual_attr;
    struct sharp_reduce_factor_attr linkage_attr[16];
};


struct vpss_3d_nr_ref_size_stat_info{
    unsigned int  ref_size_statis[3];/* 0-y used size; 1-u used size; 2-v used size*/
};

struct vpss_ccm {
    unsigned short cc_enable; //颜色校正使能
    unsigned short cc_cnoise_yth; //亮度控制增益
    unsigned short cc_cnoise_gain; //亮度控制增益
    unsigned short cc_cnoise_slop; //亮度控制增益
    signed short ccm[3][3]; //[-2048, 2047]
};

struct vpss_auto_ccm {
    unsigned short cc_color_temp;
    struct vpss_ccm auto_ccm_para;
};

struct vpss_ccm_ctr {
    unsigned short cc_start_gain;
    unsigned short cc_min_saturation;
    unsigned short cc_adjust_slop;
};

struct vpss_manual_ccm_attr {
    struct vpss_ccm_ctr manual_ccm_ctrl;
    struct vpss_ccm manual_ccm_para;
};

struct vpss_auto_ccm_attr {
    unsigned short color_matrix_num;
    struct vpss_ccm_ctr auto_ccm_ctrl;
    struct vpss_auto_ccm auto_ccm_para[10];
};

struct vpss_ccm_attr {
    enum vpss_isp_op_type cc_mode; //颜色校正矩阵联动或者手动
    struct vpss_manual_ccm_attr manual_ccm;
    struct vpss_auto_ccm_attr auto_ccm; //四个联动矩阵
};

struct vpss_hue {
    unsigned short hue_sat_en; // hue使能
    signed char hue_lut_a[65]; //[-128, 127]
    signed char hue_lut_b[65]; //[-128, 127]
    unsigned char hue_lut_s[65]; //[0, 255]
};

struct vpss_auto_hue {
    unsigned short hue_color_temp;
    struct vpss_hue auto_hue_para;
};

struct vpss_auto_hue_attr {
    unsigned short hue_num;
    struct vpss_auto_hue auto_hue_para[10];
};

struct vpss_hue_attr {
    enum vpss_isp_op_type hue_mode; //联动或者手动
    struct vpss_hue manual_hue;
    struct vpss_auto_hue_attr auto_hue; //四个联动参数
};

struct vpss_gb {
    unsigned short gb_enable; //使能位
    unsigned short gb_en_th; //[0,255]
    unsigned short gb_kstep; //[0,15]
    unsigned short gb_threshold; //[0,1023
};

struct vpss_gb_attr {
    enum vpss_isp_op_type gb_mode; //模式选择，手动或者联动
    struct vpss_gb manual_gb;
    struct vpss_gb linkage_gb[16];
};

struct vpss_ddpc {
    unsigned short ddpc_enable; //动态坏点使能位
    unsigned short ddpc_th_base; // 10bit
    unsigned short ddpc_th_slop;
    unsigned short ddpc_strength;
    unsigned short white_dpc_enable; //白点消除使能位
    unsigned short black_dpc_enable; //黑点消除使能位
};

struct vpss_ddpc_attr {
    enum vpss_isp_op_type ddpc_mode; //模式选择，手动或者联动
    struct vpss_ddpc manual_ddpc;
    struct vpss_ddpc linkage_ddpc[16];
};

struct vpss_nr1 {
    unsigned short nr1_enable; //ä½¿èƒ½ä½
    unsigned short nr1_k; //[0,15]
    unsigned short nr_keepth;
    unsigned short nr1_calc_g_k;
    unsigned short nr1_calc_r_k;
    unsigned short nr1_calc_b_k;
    unsigned short nr1_lc_lut[17]; // 10bit
    unsigned short nr1_lc_lut_key[16];
    unsigned short nr1_weight_rtbl[17]; // 10bit
    unsigned short nr1_weight_gtbl[17]; // 10bit
    unsigned short nr1_weight_btbl[17]; // 10bit
};

struct vpss_nr1_attr {
    enum vpss_isp_op_type nr1_mode; // nr1 模式，自动或者联动模式
    struct vpss_nr1 manual_nr1;
    struct vpss_nr1 linkage_nr1[16]; //联动参数
};

struct vpss_nr2 {
    unsigned short nr2_enable;
    unsigned short nr2_pass2_enable;
    unsigned short nr2_k; //[0,15]
    unsigned short ynr_keep_th;
    unsigned short ynr_intensity1; //[0, 15],default:15
    unsigned short ynr_intensity2; //[0, 15],default:15
    unsigned short nr2_calc_y_k;
    unsigned short nr2_weight_tbl[17]; // 10bit

    unsigned short ynr_y_dpc_enable;
    unsigned short ynr_y_dpc_th;
    unsigned short ynr_y_dpc_5_5;
    unsigned short ynr_y_black_dpc_enable;
    unsigned short ynr_y_white_dpc_enable;
};

struct vpss_nr2_attr {
    enum vpss_isp_op_type nr2_mode; //手动或者联动模式
    struct vpss_nr2 manual_nr2;
    struct vpss_nr2 linkage_nr2[16];
};

struct vpss_uvnr {
    unsigned short uvnr_enable; //使能位
    unsigned short uvnr_pass2_enable;
    unsigned short uvnr_k;
    unsigned short uvnr_calc_k;
    unsigned short uvnr_weight_rtbl[17]; // 10bit
};

struct vpss_uvnr_attr {
    enum vpss_isp_op_type uvnr_mode; //手动或者联动模式
    struct vpss_uvnr manual_uvnr;
    struct vpss_uvnr linkage_uvnr[16];
};

struct vpss_sharp {
    unsigned short mf_hpf_k; //[0,127]
    unsigned short mf_hpf_shift; //[0,15]

    unsigned short hf_hpf_k; //[0,127]
    unsigned short hf_hpf_shift; //[0,15]

    unsigned short sharp_method; //[0,3]
    unsigned short sharp_debug_output; //[0,3]
    unsigned short sharp_skin_gain_weaken; //[0ï¼Œ3]

    unsigned short sharp_skin_gain_th; //[0, 255]
    unsigned short sharp_skin_detect_enable;

    unsigned short ysharp_enable; //[0,1]
    unsigned short ysharp_nr_enable; //[0,1]
    unsigned short ysharp_nr_k;
    unsigned short ysharp_nrweight_tbl[17];
    unsigned short ysharp_nr_calc_k;
    unsigned short sharp_reduce_factor[24][32];

    unsigned short edge_enable;
    unsigned short Vedge_k; //[0, 15]
    unsigned short Vedge_mix_th; //[0, 255]
    unsigned short Vedge_mix_slop; //[0, 15]
    unsigned short VedgaGainW_ob; //[0, 255]
    unsigned short VedgaGainW_k; //[0, 255]
    unsigned short VedgaGainB_ob; //[0, 255]
    unsigned short VedgaGainB_k; //[0, 255]

    unsigned short Hedge_k; //[0, 15]
    unsigned short Hedge_mix_th; //[0, 255]
    unsigned short Hedge_mix_slop; //[0, 15]
    unsigned short HedgaGainW_ob; //[0, 255]
    unsigned short HedgaGainW_k; //[0, 255]
    unsigned short HedgaGainB_ob; //[0, 255]
    unsigned short HedgaGainB_k; //[0, 255]

    unsigned short TLedge_k; //[0, 15]
    unsigned short TLedgaGainW_ob; //[0, 255]
    unsigned short TLedgaGainW_k; //[0, 255]
    unsigned short TLedgaGainB_ob; //[0, 255]
    unsigned short TLedgaGainB_k; //[0, 255]

    unsigned short TRedge_k; //[0, 15]
    unsigned short TRedgaGainW_ob; //[0, 255]
    unsigned short TRedgaGainW_k; //[0, 255]
    unsigned short TRedgaGainB_ob; //[0, 255]
    unsigned short TRedgaGainB_k; //[0, 255]

    unsigned short EdgeGainMax; //[0, 511]

    unsigned short Edge_mix_th; //[0, 255]
    unsigned short Edge_mix_slop; //[0, 15]

    signed short MF_HPF_LUT[256]; //[-256,255]
    signed short HF_HPF_LUT[256]; //[-256,255]
    unsigned short MF_LUT_KEY[16];
    unsigned short HF_LUT_KEY[16];
};

struct vpss_sharp_attr {
    enum vpss_isp_op_type ysharp_mode;
    struct vpss_sharp manual_sharp_attr;
    struct vpss_sharp linkage_sharp_attr[16];
};

struct vpss_fcs {
    unsigned short fcs_th; //[0, 255]
    unsigned short fcs_gain_slop; //[0,63]
    unsigned short fcs_enable; //使能位
    unsigned short fcs_uv_nr_enable; //使能位
    unsigned short fcs_uv_nr_th; //[0, 1023]
};

struct vpss_fcs_attr {
    enum vpss_isp_op_type fcs_mode; //模式选择，手动或者联动
    struct vpss_fcs manual_fcs;
    struct vpss_fcs linkage_fcs[16];
};

struct vpss_saturation {
    unsigned short SE_enable; // 使能位
    unsigned short SE_th1; //[0, 1023]
    unsigned short SE_th2; //[0, 1023]
    unsigned short SE_th3; //[0, 1023]
    unsigned short SE_th4; //[0, 1023]
    unsigned short SE_scale_slop1; //[0, 255]
    unsigned short SE_scale_slop2; //[0, 255]
    unsigned short SE_scale1; //[0,255]
    unsigned short SE_scale2; //[0,255]
    unsigned short SE_scale3; //[0,255]
};

struct vpss_saturation_attr {
    enum vpss_isp_op_type SE_mode; //饱和度模式
    struct vpss_saturation manual_sat;
    struct vpss_saturation linkage_sat[16];
};

struct vpss_raw_lut_attr {
    unsigned short raw_gamma_enable;
    unsigned short raw_rgain;
    unsigned short raw_ggain;
    unsigned short raw_bgain;
    unsigned short raw_rgain_rev;
    unsigned short raw_ggain_rev;
    unsigned short raw_bgain_rev; // 1
    unsigned short r_key[16];
    unsigned short g_key[16];
    unsigned short b_key[16];
    unsigned short raw_r[129]; // 10bit
    unsigned short raw_g[129]; // 10bit
    unsigned short raw_b[129]; // 10bit
};

/********************** public *******************************/

/**
 * ak_vpss_get_version - get vpss version
 * return: version string
 */
const char *ak_vpss_get_version(void);

/**
 * ak_vpss_get_ispsdk_version - get ispsdk version
 * return: version string
 * notes:
 */
const char* ak_vpss_get_ispsdk_version(void);


/********************** md *******************************/

/**
 * ak_vpss_md_get_stat: get motion detection stat params
 * @dev[IN]: dev id
 * @md[OUT]: md params
 * return: 0 success, otherwise error code
 * notes:
 */
int ak_vpss_md_get_stat(int dev, struct vpss_md_info *md);

/********************** od *******************************/

/**
 * ak_vpss_od_get: get occlusion detection params
 * @dev[IN]: dev id
 * @od[OUT]: od params
 * return: 0 success, otherwise error code
 * notes:
 */
int ak_vpss_od_get(int dev, struct vpss_od_info *od);


/********************** af stat *******************************/

/**
 * ak_vpss_af_get_stat: get af stat info
 * @dev[IN]: dev id
 * @af_stat[OUT]: af_stat info
 * return: 0 success, otherwise error code
 * notes:
 */
int ak_vpss_af_get_stat(int dev, struct vpss_af_stat_info *af_stat);

/**
 * ak_vpss_af_set_attr: set af attr
 * @dev[IN]: dev id
 * @af_stat[IN]: af_attr
 * return: 0 success, otherwise error code;
 * notes:
 */
int ak_vpss_af_set_attr(int dev, struct vpss_af_attr *af_attr);

/**
 * ak_vpss_af_get_attr: set af attr
 * @dev[IN]: dev id
 * @af_stat[OUT]: af_attr
 * return: 0 success, otherwise error code;
 * notes:
 */
int ak_vpss_af_get_attr(int dev, struct vpss_af_attr *af_attr);


/********************** effect *******************************/

/**
 * ak_vpss_effect_get - get isp effect param.
 * @dev[IN]: dev id
 * @type[IN]:   effect type name
 * @value[OUT]: effect value(s)
 * return: 0 - success; otherwise error code
 */
int ak_vpss_effect_get(int dev, enum vpss_effect_type type, int *value);

/**
 * ak_vpss_effect_set - set isp effect param.
 * @dev[IN]: dev id
 * @type[IN]:   effect type name
 * @value[IN]: effect value
 * return: 0 - success; otherwise error code
 */
int ak_vpss_effect_set(int dev, enum vpss_effect_type type, const int value);

/********************** ccm *******************************/

/**
 * ak_vpss_ccm_set_attr - set isp ccm param.
 * @dev[IN]: dev id
 * @ccm_attr[IN]: ccm_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_ccm_set_attr(int dev, struct vpss_ccm_attr *ccm_attr);

/**
 * ak_vpss_ccm_get_attr - get isp ccm param.
 * @dev[IN]: dev id
 * @ccm_attr[OUT]: ccm_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_ccm_get_attr(int dev, struct vpss_ccm_attr *ccm_attr);

/********************** hue *******************************/

/**
 * ak_vpss_hue_set_attr - set isp hue param.
 * @dev[IN]: dev id
 * @hue_attr[IN]: hue_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_hue_set_attr(int dev, struct vpss_hue_attr *hue_attr);

/**
 * ak_vpss_hue_get_attr - get isp hue param.
 * @dev[IN]: dev id
 * @hue_attr[OUT]: hue_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_hue_get_attr(int dev, struct vpss_hue_attr *hue_attr);

/********************** gb *******************************/

/**
 * ak_vpss_gb_set_attr - set isp gb param.
 * @dev[IN]: dev id
 * @gb_attr[IN]: gb_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_gb_set_attr(int dev, struct vpss_gb_attr *gb_attr);

/**
 * ak_vpss_gb_get_attr - get isp gb param.
 * @dev[IN]: dev id
 * @gb_attr[OUT]: gb_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_gb_get_attr(int dev, struct vpss_gb_attr *gb_attr);


/********************** dpc *******************************/

/**
 * ak_vpss_ddpc_set_attr - set isp dpc param.
 * @dev[IN]: dev id
 * @ddpc_attr[IN]: ddpc_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_ddpc_set_attr(int dev, struct vpss_ddpc_attr *ddpc_attr);

/**
 * ak_vpss_ddpc_get_attr - get isp dpc param.
 * @dev[IN]: dev id
 * @ddpc_attr[OUT]: ddpc_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_ddpc_get_attr(int dev, struct vpss_ddpc_attr *ddpc_attr);

/********************** nr1 *******************************/

/**
 * ak_vpss_nr1_set_attr - set isp nr1 param.
 * @dev[IN]: dev id
 * @nr1_attr[IN]: nr1_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_nr1_set_attr(int dev, struct vpss_nr1_attr *nr1_attr);

/**
 * ak_vpss_nr1_get_attr - get isp nr1 param.
 * @dev[IN]: dev id
 * @nr1_attr[OUT]: nr1_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_nr1_get_attr(int dev, struct vpss_nr1_attr *nr1_attr);

/********************** nr2 *******************************/

/**
 * ak_vpss_nr2_set_attr - set isp nr2 param.
 * @dev[IN]: dev id
 * @nr2_attr[IN]: nr2_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_nr2_set_attr(int dev, struct vpss_nr2_attr *nr2_attr);

/**
 * ak_vpss_nr2_get_attr - get isp nr2 param.
 * @dev[IN]: dev id
 * @nr2_attr[OUT]: nr2_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_nr2_get_attr(int dev, struct vpss_nr2_attr *nr2_attr);

/********************** uvnr *******************************/

/**
 * ak_vpss_uvnr_set_attr - set isp uvnr param.
 * @dev[IN]: dev id
 * @uvnr_attr[IN]: uvnr_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_uvnr_set_attr(int dev, struct vpss_uvnr_attr *uvnr_attr);

/**
 * ak_vpss_uvnr_get_attr - get isp uvnr param.
 * @dev[IN]: dev id
 * @uvnr_attr[OUT]: uvnr_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_uvnr_get_attr(int dev, struct vpss_uvnr_attr *uvnr_attr);

/********************** sharp *******************************/

/**
 * ak_vpss_sharp_set_attr - set isp sharp param.
 * @dev[IN]: dev id
 * @sharp_attr[IN]: sharp_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_sharp_set_attr(int dev, struct vpss_sharp_attr *sharp_attr);

/**
 * ak_vpss_sharp_get_attr - get isp sharp param.
 * @dev[IN]: dev id
 * @sharp_attr[OUT]: sharp_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_sharp_get_attr(int dev, struct vpss_sharp_attr *sharp_attr);

/********************** fcs *******************************/

/**
 * ak_vpss_fcs_set_attr - set isp fcs param.
 * @dev[IN]: dev id
 * @fcs_attr[IN]: fcs_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_fcs_set_attr(int dev, struct vpss_fcs_attr *fcs_attr);

/**
 * ak_vpss_fcs_get_attr - get isp fcs param.
 * @dev[IN]: dev id
 * @fcs_attr[OUT]: fcs_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_fcs_get_attr(int dev, struct vpss_fcs_attr *fcs_attr);

/********************** saturation *******************************/

/**
 * ak_vpss_saturation_set_attr - set isp saturation param.
 * @dev[IN]: dev id
 * @sat_attr[IN]: sat_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_saturation_set_attr(int dev, struct vpss_saturation_attr *sat_attr);

/**
 * ak_vpss_saturation_get_attr - get isp saturation param.
 * @dev[IN]: dev id
 * @sat_attr[OUT]: sat_attr
 * return: 0 - success; otherwise error code
 */
int ak_vpss_saturation_get_attr(int dev, struct vpss_saturation_attr *sat_attr);

/********************** raw_lut *******************************/

/**
 * ak_vpss_raw_lut_set_attr - set isp raw_lut param.
 * @dev[IN]: dev id
 * @raw_lut[IN]: raw_lut
 * return: 0 - success; otherwise error code
 */
int ak_vpss_raw_lut_set_attr(int dev, struct vpss_raw_lut_attr *raw_lut);

/**
 * ak_vpss_raw_lut_get_attr - get isp raw_lut param.
 * @dev[IN]: dev id
 * @raw_lut[OUT]: raw_lut
 * return: 0 - success; otherwise error code
 */
int ak_vpss_raw_lut_get_attr(int dev, struct vpss_raw_lut_attr *raw_lut);

/********************** mask *******************************/

/**
 * ak_vpss_mask_set_area: set main & sub channel mask area
 * @dev[IN]: dev id
 * @area[IN]: main channel mask area paramters
 * return: 0 success, other error code
 * notes:
 */
int ak_vpss_mask_set_area(int dev, const struct vpss_mask_area_info *area);

/**
 * ak_vpss_mask_get_area: get main & sub channel mask area
 * @dev[IN]: dev id
 * @area[OUT]: main channel mask area paramters
 * return: 0 success, other error code
 * notes:
 */
int ak_vpss_mask_get_area(int dev, struct vpss_mask_area_info *area);

/**
 * ak_vpss_mask_set_color: set main & sub channel mask color
 * @dev[IN]: dev id
 * @color[IN]: main & sub channel mask color paramters
 * return: 0 success, other error code
 * notes:
 */
int ak_vpss_mask_set_color(int dev, const struct vpss_mask_color_info *color);

/**
 * ak_vpss_mask_get_color: get main & sub channel mask color
 * @dev[IN]: dev id
 * @color[OUT]: main & sub channel mask color paramters
 * return: 0 success, other error code
 * notes:
 */
int ak_vpss_mask_get_color(int dev, struct vpss_mask_color_info *color);


/**
 * ak_vpss_get_fps_ctrl_stat: get fps ctrl stat
 * @dev[IN]: device id
 * @stat[OUT]:gain stat
 * @need_fps[OUT]:need fps
 * return: 0 success, otherwise error code
 * notes:for switch day high light and low light. use with ak_vpss_change_sensor_fps
 */
int ak_vpss_get_fps_ctrl_stat(int dev, enum vpss_gain_stat *stat, int *need_fps);

/**
 * ak_vpss_change_sensor_fps: change sensor fps
 * @dev[IN]: device id
 * @need_fps[IN]:need fps out by ak_vpss_get_fps_ctrl_stat
 * return:  0 success, otherwise error code
 * notes:for switch day high light and low light. use with ak_vpss_get_fps_ctrl_stat
 */
int ak_vpss_change_sensor_fps(int dev, int need_fps);

/**
 * ak_vpss_set_fps_level: set sensor fps level
 * @dev[IN]: device id
 * @fps_level[IN]:fps_level params
 * return:  0 success, otherwise error code
 * notes:
 */
int ak_vpss_set_fps_level(int dev, struct vpss_fps_level *fps_level);

/**
 * ak_vpss_get_fps_level: get sensor fps level
 * @dev[IN]: device id
 * @fps_level[OUT]:fps_level params
 * return:  0 success, otherwise error code
 * notes:
 */
int ak_vpss_get_fps_level(int dev, struct vpss_fps_level *fps_level);

/**
 * ak_vpss_get_sensor_fps: get sensor framerate
 * @dev[IN]: device id
 * @fps[OUT]: fps, sensor framerate
 * return: 0 success, otherwise error code
 * notes:
 */
int ak_vpss_get_sensor_fps(int dev, int *fps);


/**
 * ak_vpss_get_ae_run_info: get ae run info
 * @dev[IN]: device id
 * @ae_run_info[OUT]: ae run info
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_ae_run_info(int dev, struct vpss_isp_ae_run_info *ae_run_info);


/**
 * ak_vpss_set_ae_init_info: set ae init info
 * @dev[IN]: device id
 * @ae_init_info[IN]: ae init info
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_ae_init_info(int dev, struct vpss_isp_ae_init_info *ae_init_info);


/**
 * ak_vpss_get_sensor_ae_info: get sensor fast ae info
 * @dev[IN]: device id
 * @ae_init_info[OUT]: ae info
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_sensor_ae_info(int dev, struct vpss_isp_ae_init_info *ae_init_info);

/**
 * ak_vpss_get_ae_attr: get AE attr
 * @dev[IN]: device id
 * @ae_attr[OUT]: AE attr info
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_ae_attr(int dev, struct vpss_isp_ae_attr *ae_attr);

/**
 * ak_vpss_set_ae_attr: set AE attr
 * @dev[IN]: device id
 * @ae_attr[IN]: AE attr info
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_ae_attr(int dev, const struct vpss_isp_ae_attr *ae_attr);

/**
 * ak_vpss_set_ae_convergence_rate: set AE convergence rate
 * @dev[IN]: device id
 * @value[IN]: param of convergence rate
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_ae_convergence_rate(int dev, unsigned long value);

/**
 * ak_vpss_get_ae_convergence_rate: get AE convergence rate
 * @dev[IN]: device id
 * @value[OUT]: param of convergence rate
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_ae_convergence_rate(int dev, unsigned long *value);

/**
 * ak_vpss_check_ae_stable: check ae is stable or not
 * @dev[IN]: device id
 * @stable[OUT]: 1 stable, 0 not stable
 * return: 0 success, otherwise error code;
 */
int ak_vpss_check_ae_stable(int dev, int *stable);

/**
 * ak_vpss_get_me_attr: get ME attr
 * @dev[IN]: device id
 * @me_attr[OUT]: ME attr info
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_me_attr(int dev, struct vpss_isp_me_attr *me_attr);

/**
 * ak_vpss_set_me_attr: set ME attr
 * @dev[IN]: device id
 * @me_attr[IN]: ME attr info
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_me_attr(int dev, const struct vpss_isp_me_attr *me_attr);

/*
 * ak_vpss_set_ae_suspend - set ae suspend flag
 * dev[IN]: dev id
 * ae_suspend_flag[IN]: 1 suspend, 0 resume
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_ae_suspend(int dev,  int ae_suspend_flag);

/**
 * ak_vpss_get_awb_stat_info: get awb stat info
 * @dev[IN]: device id
 * @awb_stat_info[OUT]: awb stat info
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_awb_stat_info(int dev, struct vpss_isp_awb_stat_info *awb_stat_info);

/**
 * ak_vpss_get_cur_lumi: get current lum factor
 * @dev[IN]: device id
 * @lumi[OUT]: lum factor
 * return: 0 success, otherwise error code;
 * notes:
 */
int ak_vpss_get_cur_lumi(int dev, int *lumi);

/**
 * ak_vpss_set_auto_day_night_param: set auto day or night switch threshold
 * @dev[IN]: device id
 * @param[IN]: input param threshold
 * return: 0 - success; otherwise error code;
 */
int ak_vpss_set_auto_day_night_param(int dev, struct ak_auto_day_night_threshold *param);

/**
 * ak_vpss_get_auto_day_night_level: get day or night
 * @dev[IN]: device id
 * @pre_ir_level[IN]: pre status, 0 day, 1 night
 * @new_level[OUT]: new status, 0 day, 1 night
 * return: 0 - success; otherwise error code;
 */
int ak_vpss_get_auto_day_night_level(int dev, int pre_ir_level, int *new_ir_level);

/**
 * ak_vpss_set_force_anti_flicker_flag: set force anti flicker flag
 * @dev[IN]: device id
 * @force_flag[IN]: force anti flicker flag
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_force_anti_flicker_flag(int dev, int force_flag);

/**
 * ak_vpss_get_force_anti_flicker_flag: get force anti flicker flag
 * @dev[IN]: device id
 * @force_flag[OUT]: force anti flicker flag
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_force_anti_flicker_flag(int dev, int *force_flag);

/**
 * ak_vpss_set_force_anti_flicker_flag_ex: set force anti flicker flag and sub_antiflicker_value
 * @dev[IN]: device id
 * @force_flag[IN]: force anti flicker flag
 * @sub_antiflicker_value[IN]: sub_antiflicker_value suggest [64-90]
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_force_anti_flicker_flag_ex(int dev, int force_flag, int sub_antiflicker_value);

/**
 * ak_vpss_get_force_anti_flicker_flag_ex: get force anti flicker flag and sub_antiflicker_value
 * @dev[IN]: device id
 * @force_flag[OUT]: force anti flicker flag
 * @sub_antiflicker_value[OUT]: sub_antiflicker_value
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_force_anti_flicker_flag_ex(int dev, int *force_flag, int *sub_antiflicker_value);

/**
 * ak_vpss_set_anti_flicker_strength: set anti flicker strength
 * @dev[IN]: device id
 * @strength[IN]: anti flicker strength [0-100], 0 means disable anti flicker
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_anti_flicker_strength(int dev, int strength);


/**
 * ak_vpss_get_anti_flicker_strength: get anti flicker strength
 * @dev[IN]: device id
 * @strength[OUT]: anti flicker strength
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_anti_flicker_strength(int dev, int *strength);

/**
 * ak_vpss_get_wdr_attr: get wdr attr
 * @dev[IN]: device id
 * @p_wdr[OUT]: wdr attr
 * return: 0 success, otherwise error code;
 * notes:
 */
int ak_vpss_get_wdr_attr(int dev, struct vpss_wdr_attr *p_wdr);

/**
 * ak_vpss_set_wdr_attr: get wdr attr
 * @dev[IN]: device id
 * @p_wdr[IN]: wdr attr
 * return: 0 success, otherwise error code;
 * notes:
 */
int ak_vpss_set_wdr_attr(int dev, struct vpss_wdr_attr *p_wdr);


/**
 * ak_vpss_get_weight_attr: get weight attr
 * @dev[IN]: device id
 * @weight_attr[OUT]: weight_attr info
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_weight_attr(int dev, struct vpss_isp_weight_attr *weight_attr);

/**
 * ak_vpss_set_weight_attr: set weight attr
 * @dev[IN]: device id
 * @weight_attr[IN]: weight_attr info
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_weight_attr(int dev, struct vpss_isp_weight_attr *weight_attr);

/**
 * ak_vpss_get_rgb_average: get rgb average value
 * @dev[IN]: device id
 * @r_avr[OUT]: r average value
 * @g_avr[OUT]: g average value
 * @b_avr[OUT]: b average value
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_rgb_average(int dev,
        unsigned int *r_avr, unsigned int *g_avr, unsigned int *b_avr);

/**
 * ak_vpss_get_wb_type: get wb type
 * @dev[IN]: device id
 * @wb_type[OUT]: wb type
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_wb_type(int dev, struct vpss_isp_wb_type_attr *wb_type);

/**
 * ak_vpss_set_wb_type: set wb type
 * @dev[IN]: device id
 * @wb_type[IN]: wb type
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_wb_type(int dev, const struct vpss_isp_wb_type_attr *wb_type);

/**
 * ak_vpss_get_3d_nr_attr: get 3D NR attr
 * @dev[IN]: device id
 * @nr_3d_attr[OUT]: 3D NR attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_3d_nr_attr(int dev, struct vpss_isp_3d_nr_attr *nr_3d_attr);

/**
 * ak_vpss_set_3d_nr_attr: set 3D NR attr
 * @dev[IN]: device id
 * @nr_3d_attr[IN]: 3D NR attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_3d_nr_attr(int dev, const struct vpss_isp_3d_nr_attr *nr_3d_attr);

/**
 * ak_vpss_get_mwb_attr: get mwb attr
 * @dev[IN]: device id
 * @mwb_attr[OUT]: mwb attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_mwb_attr(int dev, struct vpss_isp_mwb_attr *mwb_attr);

/**
 * ak_vpss_set_mwb_attr: set mwb attr
 * @dev[IN]: device id
 * @mwb_attr[IN]: mwb attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_mwb_attr(int dev, const struct vpss_isp_mwb_attr *mwb_attr);

/**
 * ak_vpss_get_awb_attr: get awb attr
 * @dev[IN]: device id
 * @awb_attr[OUT]: awb attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_awb_attr(int dev, struct vpss_isp_awb_attr *awb_attr);

/**
 * ak_vpss_set_awb_attr: set awb attr
 * @dev[IN]: device id
 * @awb_attr[IN]: awb attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_awb_attr(int dev, const struct vpss_isp_awb_attr *awb_attr);

/**
 * ak_vpss_get_exp_type: get exp type
 * @dev[IN]: device id
 * @exp_type[OUT]: exp type
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_exp_type(int dev, struct vpss_isp_exp_type *exp_type);

/**
 * ak_vpss_set_exp_type: set exp type
 * @dev[IN]: device id
 * @exp_type[IN]: exp type
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_exp_type(int dev, const struct vpss_isp_exp_type *exp_type);

/**
 * ak_vpss_get_sensor_reg: get sensor register info
 * @dev[IN]: device id
 * @sensor_reg_info[IN/OUT]: sensor register info
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_sensor_reg(int dev, struct vpss_isp_sensor_reg_info *sensor_reg_info);

/**
 * ak_vpss_get_lsc_attr: get lsc attr
 * @dev[IN]: device id
 * @lsc[OUT]: lsc attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_lsc_attr(int dev, struct vpss_lsc_attr *lsc);

/**
 * ak_vpss_set_lsc_attr: set lsc attr
 * @dev[IN]: device id
 * @lsc[IN]: lsc attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_lsc_attr(int dev, const struct vpss_lsc_attr *lsc);

/**
 * ak_vpss_get_ex_zweight_attr: get ex zweight attr
 * @dev[IN]: device id
 * @ex_zweight[OUT]: ex zweight attr info
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_ex_zweight_attr(int dev, struct vpss_isp_ex_zweight_attr *ex_zweight);

/**
 * ak_vpss_set_ex_zweight_attr: set ex zweight attr
 * @dev[IN]: device id
 * @ex_zweight[IN]: ex zweight attr info
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_ex_zweight_attr(int dev, struct vpss_isp_ex_zweight_attr *ex_zweight);

/**
 * ak_vpss_set_yuv_effect_attr: set yuv effect attr
 * @dev[IN]: device id
 * @yuv_effect[IN]: yuv_effect attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_yuv_effect_attr(int dev, const struct vpss_yuv_effect_attr *yuv_effect);

/**
 * ak_vpss_get_yuv_effect_attr: get yuv effect attr
 * @dev[IN]: device id
 * @yuv_effect[OUT]: yuv_effect attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_yuv_effect_attr(int dev, struct vpss_yuv_effect_attr *yuv_effect);

/**
 * ak_vpss_set_rgb_gamma_attr: set rgb gamma attr
 * @dev[IN]: device id
 * @rgb_gamma[IN]: rgb_gamma attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_rgb_gamma_attr(int dev, const struct vpss_rgb_gamma_attr *rgb_gamma);

/**
 * ak_vpss_get_rgb_gamma_attr: get rgb gamma attr
 * @dev[IN]: device id
 * @rgb_gamma[OUT]: rgb_gamma attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_rgb_gamma_attr(int dev, struct vpss_rgb_gamma_attr *rgb_gamma);

/**
 * ak_vpss_set_contrast_attr: set contrast attr
 * @dev[IN]: device id
 * @contrast[IN]: contrast attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_contrast_attr(int dev, const struct vpss_contrast_attr *contrast);

/**
 * ak_vpss_get_contrast_attr: get contrast attr
 * @dev[IN]: device id
 * @contrast[OUT]: contrast attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_contrast_attr(int dev, struct vpss_contrast_attr *contrast);

/**
 * ak_vpss_set_lce_attr: set lce attr
 * @dev[IN]: device id
 * @lce[IN]: lce attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_lce_attr(int dev, const struct vpss_lce_attr *lce);

/**
 * ak_vpss_get_lce_attr: get lce attr
 * @dev[IN]: device id
 * @lce[OUT]: lce attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_lce_attr(int dev, struct vpss_lce_attr *lce);

/**
 * ak_vpss_set_y_gamma_attr: set y_gamma attr
 * @dev[IN]: device id
 * @y_gamma[IN]: y_gamma attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_y_gamma_attr(int dev, const struct vpss_y_gamma_attr *y_gamma);

/**
 * ak_vpss_get_y_gamma_attr: set y_gamma attr
 * @dev[IN]: device id
 * @y_gamma[OUT]: y_gamma attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_y_gamma_attr(int dev, struct vpss_y_gamma_attr *y_gamma);

/**
 * ak_vpss_set_sharp_reduce_factor: set sharp_reduce_factor
 * @dev[IN]: device id
 * @attr[IN]: sharp_reduce_factor attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_set_sharp_reduce_factor(int dev, struct vpss_sharp_reduce_factor_attr *attr);

/**
 * ak_vpss_get_sharp_reduce_factor: get sharp_reduce_factor
 * @dev[IN]: device id
 * @attr[OUT]: sharp_reduce_factor attr
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_sharp_reduce_factor(int dev, struct vpss_sharp_reduce_factor_attr *attr);


#if 0
/**
 * ak_vpss_get_awb_stable: get awb stable flag
 * @dev[IN]: device id
 * @exp_type[OUT]: awb stable flag
 * return: 0 success, otherwise error code;
 */
int ak_vpss_get_awb_stable(int dev, char *awb_stable_flag);
#endif

/**
 * brief: set flip mirror compare to default value
 * @dev[IN]: device id
 * @flip_en[IN]:flip enable or not , 0 disable (default value), 1 enable
 * @mirror_en[IN]:mirror enable or not , 0 disable (default value), 1 enable
 * return:  0 success, otherwise error code
 * notes:
 */
int ak_vpss_switch_flip_mirror(int dev, int flip_en, int mirror_en);

/**
 * brief: get flip mirror compare to default value
 * @dev[IN]: device id
 * @flip_en[OUT]:flip enable or not , 0 disable (equal to default value), \
 * 1 enable (not equal to default value)
 * @mirror_en[OUT]:mirror enable or not , 0 disable (equal to default value), \
 * 1 enable (not equal to default value)
 * return:  0 success, otherwise error code
 * notes:
 */
int ak_vpss_get_flip_mirror(int dev, int *flip_en, int *mirror_en);

/**
 * ak_vpss_set_soft_ps_param: set soft ps switch threshold
 * @dev[IN]: device id
 * @param[IN]: input param threshold
 * return: 0 - success; otherwise error code;
 */
int ak_vpss_set_soft_ps_param(int dev, struct ak_soft_ps_attr *param);


/**
 * ak_vpss_get_soft_ps_level: get day or night
 * @dev[IN]: device id
 * @pre_ir_level[IN]: pre status, 0 day, 1 night
 * @new_level[OUT]: new status, 0 day, 1 night
 * return: 0 - success; otherwise error code;
 */
int ak_vpss_get_soft_ps_level(int dev, int pre_ir_level, int *new_ir_level);


/**
 * ak_vpss_get_soft_wh_level: get day or night
 * @dev[IN]: device id
 * @pre_ir_level[IN]: pre status, 0 day, 1 white led night
 * @new_level[OUT]: new status, 0 day, 1 white led night
 * return: 0 - success; otherwise error code;
 */
int ak_vpss_get_soft_wh_level(int dev, int pre_ir_level, int *new_ir_level);


/**
 * ak_vpss_wait_exp_stable
 * @dev[IN]: device id
 * @exp_stable_check_frame_num[IN]: exp_stable_check_frame_num
 * return: 0 - success, otherwise error code;
 * notes:
 */
int ak_vpss_wait_exp_stable(int dev, unsigned int exp_stable_check_frame_num,unsigned int timeout_ms);

/**
 * ak_vpss_get_ev: get ev
 * @dev[IN]: device id
 * @ev[OUT]: ev value
 * return: 0 - success; otherwise error code;
 */
int ak_vpss_get_ev(int dev, unsigned int *ev);

/**
 * ak_vpss_get_rgb_dis: get rgb dis
 * @dev[IN]: device id
 * @gain_r[IN]: gain_r
 * @offset_r[IN]: offset_r
 * @gain_b[IN]: gain_b
 * @offset_b[IN]: offset_b
 * @dis[OUT]: dis value
 * return: 0 - success; otherwise error code;
 */
int ak_vpss_get_rgb_dis(int dev, unsigned int gain_r, int offset_r, unsigned int gain_b, int offset_b, int *dis);

/**
 * ak_vpss_get_blk_lumi: get block lumi
 * @dev[IN]: device id
 * @blk_lumi[OUT]: block lumi[24][32]
 * return: 0 - success; otherwise error code;
 */
int ak_vpss_get_blk_lumi(int dev, unsigned int blk_lumi[24][32]);

/**
 * ak_vpss_get_non_infrared_ratio: get non infrared ratio
 * @dev[IN]: device id
 * @nir_attr[IN]:non_infrared_ratio_attr
 * @pnon_infrared_ratio[OUT]: non_infrared_ratio
 * return: 0 - success; otherwise error code;
 */
int ak_vpss_get_non_infrared_ratio(int dev, 
    struct ak_non_infrared_ratio_attr *nir_attr, unsigned int *pnon_infrared_ratio);

/**
 * ak_vpss_get_3d_nr_ref_size_stat: get  3dnr buf used size
 * @dev[IN]: device id
 * @ref_size[OUT]: 3dnr buf y/u/v used size
 * return: 0 - success; otherwise error code;
 */
int ak_vpss_get_3d_nr_ref_size_stat(int dev, struct vpss_3d_nr_ref_size_stat_info *ref_size);


#endif

/* end of file */
