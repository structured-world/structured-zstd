/*
 * Real C consumer of the vendored <zstd.h> against the structured-zstd C ABI.
 *
 * Catches "compiles in Rust but a genuine C consumer can't link / call it"
 * regressions: it includes ONLY the vendored header and links the built
 * library, exercising the simple, error, context, and frame-inspection
 * surface with a 1 MiB round trip. Exits 0 on success; a non-zero code marks
 * which check failed.
 *
 * Compiled + run by the `c-abi` CI job, linking via the canonical drop-in
 * name (a `libzstd.so` symlink to the built `libstructured_zstd.so`):
 *   cc -Ic-api/include c-api/tests/c_consumer.c -Ltarget/<...> -lzstd -o consumer
 */
#define ZSTD_STATIC_LINKING_ONLY /* expose the experimental frame-inspection API */
#include <zstd.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(void) {
    const size_t n = (size_t)1 << 20;
    unsigned char *input = (unsigned char *)malloc(n);
    unsigned char *out = (unsigned char *)malloc(n);
    if (!input || !out) return 2;
    for (size_t i = 0; i < n; i++) {
        input[i] = (unsigned char)((i * 2654435761u) >> 13);
    }

    size_t bound = ZSTD_compressBound(n);
    if (ZSTD_isError(bound)) {
        fprintf(stderr, "compressBound: %s\n", ZSTD_getErrorName(bound));
        return 3;
    }
    unsigned char *comp = (unsigned char *)malloc(bound);
    if (!comp) return 2;

    size_t csize = ZSTD_compress(comp, bound, input, n, 3);
    if (ZSTD_isError(csize)) {
        fprintf(stderr, "compress: %s\n", ZSTD_getErrorName(csize));
        return 4;
    }

    if (ZSTD_getFrameContentSize(comp, csize) != (unsigned long long)n) return 5;
    if (ZSTD_findFrameCompressedSize(comp, csize) != csize) return 6;

    size_t dsize = ZSTD_decompress(out, n, comp, csize);
    if (ZSTD_isError(dsize) || dsize != n || memcmp(out, input, n) != 0) {
        fprintf(stderr, "roundtrip failed\n");
        return 7;
    }

    if (ZSTD_versionNumber() != 10507) return 8;
    if (strcmp(ZSTD_versionString(), "1.5.7") != 0) return 9;

    /* Context API round trip. */
    ZSTD_CCtx *cctx = ZSTD_createCCtx();
    ZSTD_DCtx *dctx = ZSTD_createDCtx();
    if (!cctx || !dctx) return 10;
    size_t c2 = ZSTD_compressCCtx(cctx, comp, bound, input, n, 5);
    /* Validate c2 before it is reused as srcSize: on error it is an
       error-encoded size_t, and passing that to decompress would feed a bogus
       (huge) length and read out of bounds. */
    if (ZSTD_isError(c2)) {
        fprintf(stderr, "compressCCtx: %s\n", ZSTD_getErrorName(c2));
        return 11;
    }
    size_t d2 = ZSTD_decompressDCtx(dctx, out, n, comp, c2);
    if (ZSTD_isError(d2) || d2 != n || memcmp(out, input, n) != 0) {
        return 11;
    }
    ZSTD_freeCCtx(cctx);
    ZSTD_freeDCtx(dctx);

    /* Experimental frame-header inspection. */
    ZSTD_FrameHeader zfh;
    if (ZSTD_getFrameHeader(&zfh, comp, csize) != 0) return 12;
    if (zfh.frameContentSize != (unsigned long long)n) return 13;
    if (zfh.frameType != ZSTD_frame) return 14;

    /* Error mapping: a bad buffer must report an error code. */
    unsigned char garbage[16] = {0};
    size_t bad = ZSTD_decompress(out, n, garbage, sizeof garbage);
    if (!ZSTD_isError(bad)) return 15;

    free(input);
    free(out);
    free(comp);
    printf("c_consumer: OK (csize=%zu)\n", csize);
    return 0;
}
