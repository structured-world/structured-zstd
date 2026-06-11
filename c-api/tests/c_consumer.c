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

    /* Advanced parameter API + ZSTD_compress2. */
    cctx = ZSTD_createCCtx();
    if (!cctx) return 16;
    {
        ZSTD_bounds wb = ZSTD_cParam_getBounds(ZSTD_c_windowLog);
        if (ZSTD_isError(wb.error) || wb.lowerBound > wb.upperBound) return 17;
        if (ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_compressionLevel, 7))) return 18;
        if (ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_checksumFlag, 1))) return 19;
        if (!ZSTD_isError(ZSTD_CCtx_setParameter(cctx, ZSTD_c_windowLog, 99))) return 20;
        int got = -1;
        if (ZSTD_isError(ZSTD_CCtx_getParameter(cctx, ZSTD_c_compressionLevel, &got)) || got != 7)
            return 21;
        size_t c3 = ZSTD_compress2(cctx, comp, bound, input, n);
        if (ZSTD_isError(c3)) {
            fprintf(stderr, "compress2: %s\n", ZSTD_getErrorName(c3));
            return 22;
        }
        dctx = ZSTD_createDCtx();
        if (!dctx) return 16;
        size_t d3 = ZSTD_decompressDCtx(dctx, out, n, comp, c3);
        if (ZSTD_isError(d3) || d3 != n || memcmp(out, input, n) != 0) return 23;
        ZSTD_freeDCtx(dctx);
        if (ZSTD_isError(ZSTD_CCtx_reset(cctx, ZSTD_reset_session_and_parameters))) return 24;
    }
    ZSTD_freeCCtx(cctx);

    /* Streaming round trip: chunked compress, then chunked decompress. */
    {
        ZSTD_CStream *zcs = ZSTD_createCStream();
        ZSTD_DStream *zds = ZSTD_createDStream();
        if (!zcs || !zds) return 25;
        size_t out_cap = ZSTD_CStreamOutSize();
        /* calloc: the streaming calls initialize exactly the bytes copied
         * out afterwards, but zero-init keeps static analyzers quiet. */
        unsigned char *sbuf = (unsigned char *)calloc(1, out_cap);
        unsigned char *scomp = (unsigned char *)malloc(bound + 64);
        if (!sbuf || !scomp) return 2;
        size_t scomp_len = 0;
        const size_t chunk = 64 * 1024;
        for (size_t off = 0; off < n; off += chunk) {
            size_t len = n - off < chunk ? n - off : chunk;
            ZSTD_inBuffer inb = {input + off, len, 0};
            while (inb.pos < inb.size) {
                ZSTD_outBuffer outb = {sbuf, out_cap, 0};
                size_t rc = ZSTD_compressStream2(zcs, &outb, &inb, ZSTD_e_continue);
                if (ZSTD_isError(rc)) return 26;
                memcpy(scomp + scomp_len, sbuf, outb.pos);
                scomp_len += outb.pos;
            }
        }
        for (;;) {
            ZSTD_outBuffer outb = {sbuf, out_cap, 0};
            size_t rc = ZSTD_endStream(zcs, &outb);
            if (ZSTD_isError(rc)) return 27;
            memcpy(scomp + scomp_len, sbuf, outb.pos);
            scomp_len += outb.pos;
            if (rc == 0) break;
        }
        ZSTD_freeCStream(zcs);

        memset(out, 0, n);
        size_t restored = 0;
        ZSTD_inBuffer inb = {scomp, scomp_len, 0};
        size_t dout_cap = ZSTD_DStreamOutSize();
        unsigned char *dbuf = (unsigned char *)calloc(1, dout_cap);
        if (!dbuf) return 2;
        for (;;) {
            ZSTD_outBuffer outb = {dbuf, dout_cap, 0};
            size_t rc = ZSTD_decompressStream(zds, &outb, &inb);
            if (ZSTD_isError(rc)) return 28;
            memcpy(out + restored, dbuf, outb.pos);
            restored += outb.pos;
            if (rc == 0 && inb.pos == inb.size) break;
            if (outb.pos == 0 && inb.pos == inb.size) return 29; /* stalled */
        }
        ZSTD_freeDStream(zds);
        if (restored != n || memcmp(out, input, n) != 0) return 30;
        free(sbuf);
        free(scomp);
        free(dbuf);
    }

    free(input);
    free(out);
    free(comp);
    printf("c_consumer: OK (csize=%zu)\n", csize);
    return 0;
}
