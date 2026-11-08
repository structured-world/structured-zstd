/*
 * Extensions this library adds to the libzstd API. zstd.h, zdict.h and
 * zstd_errors.h stay upstream's verbatim; what goes beyond them is declared
 * here. A program that includes only upstream's headers builds and links
 * unchanged.
 *
 * Extensions are functions, never new ZSTD_cParameter values: those numbers
 * are upstream's to assign.
 */
#ifndef STRUCTURED_ZSTD_H
#define STRUCTURED_ZSTD_H

#include <stddef.h>
#include "zstd.h"

#if defined (__cplusplus)
extern "C" {
#endif

/* Raw skip
 * Whether a block whose sample reads as incompressible may go out raw, and a
 * block of one repeated byte as RLE, without being searched. The value is a
 * ZSTD_ParamSwitch_e (declared in zstd.h under ZSTD_STATIC_LINKING_ONLY):
 *   0 (ZSTD_ps_auto, the default) or 1 (ZSTD_ps_enable): the skip is on;
 *   2 (ZSTD_ps_disable): every block is searched, as upstream searches it, and
 *     only a block the search fails to shrink goes out raw.
 * Any other value fails with ZSTD_error_parameter_outOfBound. The context's
 * setting also reaches a frame a referenced CDict drives. Setting it while a
 * frame streams fails with ZSTD_error_stage_wrong, as a parameter does.
 * Each function returns 0 or an error code (ZSTD_isError). */
ZSTDLIB_API size_t ZSTD_CCtx_setRawSkip(ZSTD_CCtx* cctx, int value);
ZSTDLIB_API size_t ZSTD_CCtx_getRawSkip(const ZSTD_CCtx* cctx, int* value);
/* `struct ZSTD_CCtx_params_s` is ZSTD_CCtx_params, which zstd.h names only
 * under ZSTD_STATIC_LINKING_ONLY. */
struct ZSTD_CCtx_params_s;
ZSTDLIB_API size_t ZSTD_CCtxParams_setRawSkip(struct ZSTD_CCtx_params_s* params, int value);
ZSTDLIB_API size_t ZSTD_CCtxParams_getRawSkip(const struct ZSTD_CCtx_params_s* params, int* value);

#if defined (__cplusplus)
}
#endif

#endif /* STRUCTURED_ZSTD_H */
