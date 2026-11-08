/*
 * Extensions this library adds to the libzstd API. zstd.h, zdict.h and
 * zstd_errors.h stay upstream's verbatim; what goes beyond them is declared
 * here. A program that includes only upstream's headers builds and links
 * unchanged.
 */
#ifndef STRUCTURED_ZSTD_H
#define STRUCTURED_ZSTD_H

#include "zstd.h"

#if defined (__cplusplus)
extern "C" {
#endif

/* ZSTD_c_rawSkip
 * Takes a ZSTD_ParamSwitch_e value, outside the range upstream numbers its
 * own parameters in.
 *
 * Controls whether a block whose sample reads as incompressible may go out
 * raw, and a block of one repeated byte as RLE, without being searched.
 * ZSTD_ps_auto (the default) and ZSTD_ps_enable allow it; ZSTD_ps_disable
 * searches every block, as upstream does, and only a block the search fails
 * to shrink goes out raw. It also reaches a frame a referenced CDict drives.
 */
#define ZSTD_c_rawSkip ((ZSTD_cParameter)2000)

#if defined (__cplusplus)
}
#endif

#endif /* STRUCTURED_ZSTD_H */
