// Decode btrparts (zstd frames) with a libzstd built several ways, to see what the kernel's build
// costs. Frames come from FILE (concatenated frames, sizes in FILE.sizes: "stored unencoded" per
// line). Modes: oneshot (ZSTD_decompressDCtx into a 128 KiB buffer) and stream4k (the btrfs pattern:
// ZSTD_decompressStream, input fed in 4 KiB pieces, output 4 KiB at a time, then memcpy'd into a
// destination page).
#define ZSTD_STATIC_LINKING_ONLY
#include "zstd.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }

int main(int argc, char **argv)
{
	if (argc < 4) { fprintf(stderr, "usage: %s FRAMES SIZES ROUNDS\n", argv[0]); return 2; }
	FILE *f = fopen(argv[1], "rb"); fseek(f, 0, SEEK_END); long n = ftell(f); rewind(f);
	unsigned char *all = malloc(n); if (fread(all, 1, n, f) != (size_t)n) return 1; fclose(f);
	FILE *s = fopen(argv[2], "r"); size_t cap = 1 << 16, cnt = 0; size_t *cs = malloc(cap * sizeof *cs), *us = malloc(cap * sizeof *us);
	while (fscanf(s, "%zu %zu", &cs[cnt], &us[cnt]) == 2) if (++cnt == cap) { cap *= 2; cs = realloc(cs, cap * sizeof *cs); us = realloc(us, cap * sizeof *us); }
	int rounds = atoi(argv[3]); const char *mode = argc > 4 ? argv[4] : "os";
	ZSTD_DCtx *dctx = ZSTD_createDCtx(); ZSTD_DStream *ds = ZSTD_createDStream();
	static unsigned char out[1 << 17], page[4096], scratch[4096];
	size_t total = 0; for (size_t i = 0; i < cnt; i++) total += us[i];
	double best1 = 1e9, best2 = 1e9;
	for (int r = 0; r < rounds; r++) {
		double t = now(); size_t off = 0;
		if (strchr(mode, 'o')) for (size_t i = 0; i < cnt; i++) {
			size_t z = ZSTD_decompressDCtx(dctx, out, sizeof out, all + off, cs[i]);
			if (ZSTD_isError(z) || z != us[i]) { fprintf(stderr, "frame %zu: %s\n", i, ZSTD_getErrorName(z)); return 1; }
			off += cs[i];
		}
		t = now() - t; if (t < best1) best1 = t;
		t = now(); off = 0;
		if (strchr(mode, 's')) for (size_t i = 0; i < cnt; i++) {
			ZSTD_initDStream(ds);
			ZSTD_inBuffer in = { all + off, cs[i] < 4096 ? cs[i] : 4096, 0 };
			size_t fed = in.size, got = 0;
			for (;;) {
				ZSTD_outBuffer o = { scratch, 4096, 0 };
				size_t z = ZSTD_decompressStream(ds, &o, &in);
				if (ZSTD_isError(z)) { fprintf(stderr, "stream %zu: %s\n", i, ZSTD_getErrorName(z)); return 1; }
				memcpy(page, scratch, o.pos); got += o.pos;
				if (z == 0) break;
				if (in.pos == in.size && fed < cs[i]) { size_t k = cs[i] - fed < 4096 ? cs[i] - fed : 4096; in.src = all + off + fed; in.size = k; in.pos = 0; fed += k; }
			}
			if (got != us[i]) { fprintf(stderr, "stream %zu: %zu of %zu\n", i, got, us[i]); return 1; }
			off += cs[i];
		}
		t = now() - t; if (t < best2) best2 = t;
	}
	printf("oneshot %.0f MB/s  stream4k %.0f MB/s  (%zu frames, %.1f MB)\n", total / best1 / 1e6, total / best2 / 1e6, cnt, total / 1e6);
	return 0;
}
