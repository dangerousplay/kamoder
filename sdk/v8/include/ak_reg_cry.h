#ifndef _AK_REG_CRY_H_
#define _AK_REG_CRY_H_

/**
 * ak_cry_get_version -  get cry version
 * return: version string
 * notes:
 */
const char *ak_cry_get_version(void);


/**
 * ak_cry_register -  register cry
 * return: 0 success, other failed
 * notes:
 */
int ak_cry_register(void);


#endif

