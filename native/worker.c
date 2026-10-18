/* 一次性图片编码子进程：按网关解析好的一段 OSS 处理串加工一张图。
 *
 * 编解码全部交给 libvips 及其原生 codec（JPEG 质量估算借 MagickWand，HEIC/AVIF 编码直接调 libheif）。
 * 本进程不持有 URL、Host 或凭据，只读父进程建的私有输入文件、写私有输出文件；资源上限在加载第三方
 * 图片前生效，超时/内存/文件上限由内核终止，父进程回收退出状态和临时目录。
 *
 * 用法（参数由 src/transform.rs 的 step_args 生成，已全部校验）：
 *   image-worker <输入> <输出> <源格式> <转正0|1> <产物image|info|hue> <输出格式|source>
 *                <质量方式none|rel|abs> <质量值> <渐进0|1> [操作 参数…]…
 * 操作与参数个数：
 *   resize <m> <w> <h> <l> <s> <p> <limit> <RRGGBB>   crop <w> <h> <x> <y> <g>
 *   rotate <角度>  circle <r>  rounded <r>  indexcrop <x|y> <边长> <序号>
 *   blur <r> <s>  bright <v>  contrast <v>  sharpen <v>
 * 标准输出：image <宽> <单帧高> <格式> | info（字段写入输出文件）| hue <R> <G> <B>。
 * 退出码：0 成功；2 参数错误；3 初始化失败；4 解码/处理/编码失败；5 源图边长超限；
 *         6 带透明的 HEIC 不能存成 JPEG；7 旋转的图超过 4096；8 缩放结果超限；9 WebP 输出超限；
 *         10 HEIC/AVIF 输出超限；11 裁剪起点在图外；12 源图总像素超限。
 */
#include <vips/vips.h>
#include <libheif/heif.h>
/* bookworm（生产镜像）是 ImageMagick 6，trixie（本地测试环境）是 7，头文件路径不同。 */
#if __has_include(<MagickWand/MagickWand.h>)
#include <MagickWand/MagickWand.h>
#else
#include <wand/MagickWand.h>
#endif
#include <sys/resource.h>
#include <unistd.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>

/* OSS 源图总像素上限 2.5 亿（动图按 宽×高×帧数 计）。 */
#define MAX_SOURCE_PIXELS 250000000ULL
/* OSS 缩放结果上限：单边 16384、总像素 16777216（=4096×4096）。 */
#define MAX_RESIZE_SIDE 16384
#define MAX_RESIZE_PIXELS 16777216ULL
/* OSS 旋转的源图单边上限。 */
#define MAX_ROTATE_SIDE 4096
/* OSS 目标格式上限：WebP 单边 16383；HEIC 单边 4096；AVIF 宽 4096、总像素 4096×2304。 */
#define MAX_WEBP_SIDE 16383
#define MAX_HEIC_SIDE 4096
#define MAX_AVIF_WIDTH 4096
#define MAX_AVIF_PIXELS 9437184ULL
/* 单个子进程的资源上限：地址空间 768 MiB、CPU 25 秒、墙钟 30 秒（父请求整体 55 秒）。
 * 输出文件 80 MiB：OSS 允许的最大缩放结果 16777216 像素存成 32 位 BMP 约 64 MiB，留出余量。 */
#define LIMIT_MEMORY (768UL * 1024 * 1024)
#define LIMIT_CPU_SECONDS 25
#define LIMIT_WALL_SECONDS 30
#define LIMIT_OUTPUT_BYTES (80UL * 1024 * 1024)

static int fail(int code) { vips_error_clear(); return code; }

/* 帧列表：动图（GIF/WebP 多帧且结果仍是动图）每帧一张，其余只有一张。 */
typedef struct {
    VipsImage **frame; /* 每帧图像，归本结构所有 */
    int count;         /* 帧数，≥1 */
} Frames;

/* 用新图替换第 i 帧（接管 next 的引用）。 */
static void replace(Frames *f, int i, VipsImage *next) {
    g_object_unref(f->frame[i]);
    f->frame[i] = next;
}

/* 动图只在 resize/crop 后保持动态（OSS：其他操作把动图变成静态图）；其余操作前先只留第一帧。 */
static void first_frame_only(Frames *f) {
    for (int i = 1; i < f->count; i++) g_object_unref(f->frame[i]);
    f->count = 1;
}

/* 转成 8 位 sRGB（保留透明通道），绘制蒙版、填充背景色与调色前统一像素格式。 */
static int to_srgb8(VipsImage **image) {
    VipsImage *out = NULL;
    if (vips_image_guess_interpretation(*image) != VIPS_INTERPRETATION_sRGB) {
        if (vips_colourspace(*image, &out, VIPS_INTERPRETATION_sRGB, NULL)) return -1;
        g_object_unref(*image); *image = out; out = NULL;
    }
    if (vips_image_get_format(*image) != VIPS_FORMAT_UCHAR) {
        if (vips_cast(*image, &out, VIPS_FORMAT_UCHAR, "shift", TRUE, NULL)) return -1;
        g_object_unref(*image); *image = out;
    }
    return 0;
}

/* 把 0/255 蒙版并进透明通道：已有透明通道时取二者的乘积，没有时直接作为透明通道。 */
static int apply_mask(VipsImage **image, VipsImage *mask) {
    VipsImage *out = NULL;
    if (vips_image_hasalpha(*image)) {
        int bands = vips_image_get_bands(*image);
        VipsImage *colour = NULL, *alpha = NULL, *product = NULL, *scaled = NULL;
        int status = vips_extract_band(*image, &colour, 0, "n", bands - 1, NULL) ||
            vips_extract_band(*image, &alpha, bands - 1, NULL) ||
            vips_multiply(alpha, mask, &product, NULL) ||
            vips_linear1(product, &scaled, 1.0 / 255.0, 0.0, "uchar", TRUE, NULL) ||
            vips_bandjoin2(colour, scaled, &out, NULL);
        if (colour) g_object_unref(colour);
        if (alpha) g_object_unref(alpha);
        if (product) g_object_unref(product);
        if (scaled) g_object_unref(scaled);
        if (status) return -1;
    } else if (vips_bandjoin2(*image, mask, &out, NULL)) return -1;
    g_object_unref(*image); *image = out;
    return 0;
}

/* 新建可绘制的单通道全黑蒙版。 */
static VipsImage *blank_mask(int width, int height) {
    VipsImage *lazy = NULL;
    if (vips_black(&lazy, width, height, NULL)) return NULL;
    VipsImage *memory = vips_image_copy_memory(lazy);
    g_object_unref(lazy);
    return memory;
}

/* 只对颜色通道做线性变换 out = a·in + b（透明通道原样保留），结果截断到 0–255。 */
static int linear_colour(VipsImage **image, double a, double b) {
    if (to_srgb8(image)) return -1;
    VipsImage *out = NULL;
    if (vips_image_hasalpha(*image)) {
        int bands = vips_image_get_bands(*image);
        VipsImage *colour = NULL, *alpha = NULL, *mapped = NULL;
        int status = vips_extract_band(*image, &colour, 0, "n", bands - 1, NULL) ||
            vips_extract_band(*image, &alpha, bands - 1, NULL) ||
            vips_linear1(colour, &mapped, a, b, "uchar", TRUE, NULL) ||
            vips_bandjoin2(mapped, alpha, &out, NULL);
        if (colour) g_object_unref(colour);
        if (alpha) g_object_unref(alpha);
        if (mapped) g_object_unref(mapped);
        if (status) return -1;
    } else if (vips_linear1(*image, &out, a, b, "uchar", TRUE, NULL)) return -1;
    g_object_unref(*image); *image = out;
    return 0;
}

/* resize 参数，取值已由 src/process.rs 校验。 */
typedef struct {
    const char *mode;  /* lfit / mfit / fill / pad / fixed */
    int w, h;          /* 目标宽高，0 为未指定，≤16384 */
    int l, s;          /* 目标长边/短边，0 为未指定，≤16384 */
    int p;             /* 百分比 1–1000，0 为未指定 */
    int limit;         /* 1：需要放大时保持原尺寸 */
    double color[3];   /* pad 的填充色 RGB，0–255 */
} Resize;
/* 缩放几何：先缩到 tw×th，再按 post 居中裁成或填充成 bw×bh。 */
typedef struct {
    int tw, th;        /* 缩放后的尺寸（像素） */
    int bw, bh;        /* 最终画布尺寸（像素）；无 post 时等于 tw×th */
    int post;          /* 0 无；1 居中裁剪（fill）；2 居中填充（pad） */
    int unchanged;     /* 1：受 limit 约束保持原图，不缩放 */
} Geometry;

/* 按 OSS 规则（文档 + 实测，见 docs/protocol.md）算缩放几何。 */
static void geometry(int W, int H, Resize r, Geometry *g) {
    memset(g, 0, sizeof(*g));
    double scale;
    /* mfit 不单列：除 lfit/pad 外，fill 与 mfit 都取能盖满目标框的较大缩放比。 */
    int lfit = !strcmp(r.mode, "lfit"), fill = !strcmp(r.mode, "fill");
    int pad = !strcmp(r.mode, "pad"), fixed = !strcmp(r.mode, "fixed");
    if (!r.w && !r.h && r.l && r.s) {
        /* 同时给 l 与 s：按图片方向换成宽高框，再由 m 决定行为。文档称此时始终保持比例，
         * 但 OSS 实测 m_fixed,l_200,s_100 对 400×300 输出 200×100，即 fixed 照样强制拉伸。 */
        r.w = W >= H ? r.l : r.s;
        r.h = W >= H ? r.s : r.l;
    }
    if (r.w || r.h) {
        if (!(r.w && r.h)) {
            if (fill || pad) r.w = r.h = r.w ? r.w : r.h; /* 单边的 fill/pad 按正方形框 */
            else {
                /* 单边的 lfit/mfit/fixed：按给定边等比缩放。 */
                scale = r.w ? (double)r.w / W : (double)r.h / H;
                if (r.limit && scale > 1) { g->unchanged = 1; return; }
                g->tw = g->bw = (int)lround(W * scale);
                g->th = g->bh = (int)lround(H * scale);
                goto clamp;
            }
        }
        double sx = (double)r.w / W, sy = (double)r.h / H;
        if (fixed) {
            if (r.limit && (r.w > W || r.h > H)) { g->unchanged = 1; return; }
            g->tw = g->bw = r.w; g->th = g->bh = r.h;
            goto clamp;
        }
        scale = (lfit || pad) ? fmin(sx, sy) : fmax(sx, sy);
        /* OSS 实测：pad 不受 limit 约束，内图照样放大再填充。 */
        if (r.limit && !pad && scale > 1) { g->unchanged = 1; return; }
        g->tw = (int)lround(W * scale); g->th = (int)lround(H * scale);
        if (fill) { g->post = 1; g->bw = r.w; g->bh = r.h; }
        else if (pad) { g->post = 2; g->bw = r.w; g->bh = r.h; }
        else { g->bw = g->tw; g->bh = g->th; }
    } else if (r.l || r.s) {
        /* 只给 l 或 s：按该边等比缩放，m 不起作用。 */
        scale = r.l ? (double)r.l / (W > H ? W : H) : (double)r.s / (W < H ? W : H);
        if (r.limit && scale > 1) { g->unchanged = 1; return; }
        g->tw = g->bw = (int)lround(W * scale);
        g->th = g->bh = (int)lround(H * scale);
    } else {
        /* p 百分比：不受 limit 约束（OSS 实测 p_120 直接放大）。 */
        scale = r.p / 100.0;
        g->tw = g->bw = (int)lround(W * scale);
        g->th = g->bh = (int)lround(H * scale);
    }
clamp:
    if (g->tw < 1) g->tw = 1;
    if (g->th < 1) g->th = 1;
    if (g->bw < 1) g->bw = 1;
    if (g->bh < 1) g->bh = 1;
    if (g->post == 1) { /* fill 的中间尺寸至少覆盖目标框 */
        if (g->tw < g->bw) g->tw = g->bw;
        if (g->th < g->bh) g->th = g->bh;
    }
}

/* 缩放结果（含 fill 的中间图）是否超出 OSS 上限。 */
static int too_large(int w, int h) {
    return w > MAX_RESIZE_SIDE || h > MAX_RESIZE_SIDE || (uint64_t)w * h > MAX_RESIZE_PIXELS;
}

/* 缩放后的居中裁剪或填充。 */
static int finish_resize(VipsImage **image, const Geometry *g, const Resize *r) {
    VipsImage *out = NULL;
    if (g->post == 1) {
        if (vips_extract_area(*image, &out, (g->tw - g->bw) / 2, (g->th - g->bh) / 2, g->bw, g->bh, NULL)) return -1;
    } else if (g->post == 2) {
        if (to_srgb8(image)) return -1;
        double background[4] = {r->color[0], r->color[1], r->color[2], 255};
        int bands = vips_image_get_bands(*image);
        VipsArrayDouble *ink = vips_array_double_new(background, bands >= 4 ? 4 : 3);
        int status = vips_embed(*image, &out, (g->bw - g->tw) / 2, (g->bh - g->th) / 2, g->bw, g->bh,
            "extend", VIPS_EXTEND_BACKGROUND, "background", ink, NULL);
        vips_area_unref((VipsArea *)ink);
        if (status) return -1;
    } else return 0;
    g_object_unref(*image); *image = out;
    return 0;
}

/* 按 libheif 显式限制 x265/AOM 为单线程编码 HEIC/AVIF。libvips 8.14 未暴露 HEIF 编码线程配置，
 * 默认按宿主机核数开线程池会让容器内存耗尽；这里只拷贝已处理好的 8 位像素，编码仍由 libheif 完成。 */
static int save_heif(VipsImage *image, const char *path, int quality, int avif) {
    VipsImage *rgb = NULL, *pixels = NULL;
    struct heif_context *context = NULL;
    struct heif_encoder *encoder = NULL;
    struct heif_image *frame = NULL;
    struct heif_image_handle *handle = NULL;
    struct heif_encoding_options *options = NULL;
    void *data = NULL; size_t length = 0; int status = -1;
    if (vips_colourspace(image, &rgb, VIPS_INTERPRETATION_sRGB, NULL) ||
        vips_cast(rgb, &pixels, VIPS_FORMAT_UCHAR, "shift", TRUE, NULL)) goto done;
    int width = vips_image_get_width(pixels), height = vips_image_get_height(pixels);
    int bands = vips_image_get_bands(pixels), alpha = vips_image_hasalpha(pixels);
    if (bands != (alpha ? 4 : 3)) goto done;
    data = vips_image_write_to_memory(pixels, &length);
    if (!data || length != (size_t)width * height * bands) goto done;
    context = heif_context_alloc(); if (!context) goto done;
    if (heif_context_get_encoder_for_format(context, avif ? heif_compression_AV1 : heif_compression_HEVC, &encoder).code) goto done;
    if (heif_encoder_set_lossy_quality(encoder, quality).code) goto done;
    if (avif) {
        if (heif_encoder_set_parameter_integer(encoder, "threads", 1).code ||
            heif_encoder_set_parameter_integer(encoder, "speed", 6).code) goto done;
    } else {
        if (heif_encoder_set_parameter_string(encoder, "x265:pools", "none").code ||
            heif_encoder_set_parameter_string(encoder, "x265:frame-threads", "1").code ||
            heif_encoder_set_parameter_string(encoder, "preset", "fast").code) goto done;
    }
    if (heif_image_create(width, height, heif_colorspace_RGB, alpha ? heif_chroma_interleaved_RGBA : heif_chroma_interleaved_RGB, &frame).code ||
        heif_image_add_plane(frame, heif_channel_interleaved, width, height, 8).code) goto done;
    int stride = 0; uint8_t *plane = heif_image_get_plane(frame, heif_channel_interleaved, &stride);
    if (!plane || stride < width * bands) goto done;
    for (int row = 0; row < height; row++)
        memcpy(plane + (size_t)row * stride, (uint8_t *)data + (size_t)row * width * bands, (size_t)width * bands);
    options = heif_encoding_options_alloc(); if (!options) goto done;
    options->save_alpha_channel = alpha;
    if (heif_context_encode_image(context, frame, encoder, options, &handle).code ||
        heif_context_write_to_file(context, path).code) goto done;
    status = 0;
done:
    if (options) heif_encoding_options_free(options);
    if (handle) heif_image_handle_release(handle);
    if (frame) heif_image_release(frame);
    if (encoder) heif_encoder_release(encoder);
    if (context) heif_context_free(context);
    g_free(data);
    if (pixels) g_object_unref(pixels);
    if (rgb) g_object_unref(rgb);
    return status;
}

/* info：输出头信息与 EXIF 原始字段，由父进程排成 OSS 的 JSON。只读文件头，不解码像素。 */
static int write_info(VipsImage *header, int frames, const char *output) {
    FILE *file = fopen(output, "w");
    if (!file) return 4;
    fprintf(file, "width\t%d\nheight\t%d\nframes\t%d\n", vips_image_get_width(header), vips_image_get_page_height(header), frames);
    const char *unit = NULL;
    /* 文件自带分辨率时 libvips 才会写 resolution-unit；没有这一项时不报分辨率，与 OSS 一致。 */
    if (vips_image_get_typeof(header, "resolution-unit") && !vips_image_get_string(header, "resolution-unit", &unit) && unit)
        fprintf(file, "resunit\t%s\nxres\t%.17g\nyres\t%.17g\n", unit, vips_image_get_xres(header), vips_image_get_yres(header));
    /* IFD0 先输出（同名时优先），其后 EXIF、GPS、互操作 IFD，缩略图 IFD1 最后。 */
    static const int order[] = {0, 2, 3, 4, 1};
    gchar **fields = vips_image_get_fields(header);
    for (int k = 0; k < 5; k++) {
        char prefix[16]; snprintf(prefix, sizeof(prefix), "exif-ifd%d-", order[k]);
        for (int i = 0; fields[i]; i++) {
            const char *value = NULL;
            if (strncmp(fields[i], prefix, strlen(prefix)) || vips_image_get_string(header, fields[i], &value) || !value) continue;
            /* 字段值来自图片，去掉换行和制表符，避免破坏逐行格式。 */
            fprintf(file, "exif\tifd%d\t%s\t", order[k], fields[i] + strlen(prefix));
            for (const char *c = value; *c; c++) fputc(*c == '\n' || *c == '\t' || *c == '\r' ? ' ' : *c, file);
            fputc('\n', file);
        }
    }
    g_strfreev(fields);
    return fclose(file) ? 4 : 0;
}

/* average-hue：第一帧按白底合成透明区域后各通道的平均值。OSS 的算法是“突出颜色”的平均，未公开，
 * 这里取简单平均，见 docs/protocol.md。 */
static int write_hue(const char *input) {
    VipsImage *image = vips_image_new_from_file(input, "fail_on", VIPS_FAIL_ON_ERROR, NULL);
    if (!image || to_srgb8(&image)) return 4;
    if (vips_image_hasalpha(image)) {
        VipsImage *flat = NULL; VipsArrayDouble *white = vips_array_double_newv(3, 255.0, 255.0, 255.0);
        int status = vips_flatten(image, &flat, "background", white, NULL);
        vips_area_unref((VipsArea *)white);
        if (status) return 4;
        g_object_unref(image); image = flat;
    }
    double mean[3] = {0, 0, 0};
    int bands = vips_image_get_bands(image);
    for (int b = 0; b < 3; b++) {
        VipsImage *band = NULL;
        if (vips_extract_band(image, &band, b < bands ? b : 0, NULL) || vips_avg(band, &mean[b], NULL)) return 4;
        g_object_unref(band);
    }
    printf("hue %ld %ld %ld\n", lround(mean[0]), lround(mean[1]), lround(mean[2]));
    return 0;
}

/* 源图质量：JPEG 用 ImageMagick 的量化表估算；其他格式按 95（OSS 实测 PNG 源与 95 的结果一致）。 */
static int source_quality(const char *input, const char *source) {
    if (strcmp(source, "jpg")) return 95;
    int quality = 95;
    MagickWandGenesis();
    MagickWand *wand = NewMagickWand();
    if (MagickPingImage(wand, input)) {
        size_t estimated = MagickGetImageCompressionQuality(wand);
        if (estimated > 0 && estimated <= 100) quality = (int)estimated;
    }
    DestroyMagickWand(wand);
    MagickWandTerminus();
    return quality;
}

int main(int argc, char **argv) {
    if (argc < 10) return 2;
    struct rlimit memory = {LIMIT_MEMORY, LIMIT_MEMORY};
    struct rlimit cpu = {LIMIT_CPU_SECONDS, LIMIT_CPU_SECONDS}, file = {LIMIT_OUTPUT_BYTES, LIMIT_OUTPUT_BYTES};
    if (setrlimit(RLIMIT_AS, &memory) || setrlimit(RLIMIT_CPU, &cpu) || setrlimit(RLIMIT_FSIZE, &file)) return 3;
    alarm(LIMIT_WALL_SECONDS);
    if (VIPS_INIT(argv[0])) return 3;
    vips_concurrency_set(1);
    vips_cache_set_max(0);
    const char *input = argv[1], *output = argv[2], *source = argv[3];
    int orient = atoi(argv[4]);
    const char *kind = argv[5], *format = argv[6], *quality_mode = argv[7];
    int quality_value = atoi(argv[8]), interlace = atoi(argv[9]);
    char **ops = argv + 10; int op_count = argc - 10;

    /* OSS：format,gif 只对 GIF 源生效，其他源保持原格式；没给 format 也保持原格式。 */
    if (!strcmp(format, "source") || (!strcmp(format, "gif") && strcmp(source, "gif"))) format = source;

    VipsImage *header = vips_image_new_from_file(input, "access", VIPS_ACCESS_SEQUENTIAL, "fail_on", VIPS_FAIL_ON_ERROR, NULL);
    if (!header) return fail(4);
    int width = vips_image_get_width(header), height = vips_image_get_page_height(header), pages = 1;
    if (vips_image_get_typeof(header, VIPS_META_N_PAGES)) vips_image_get_int(header, VIPS_META_N_PAGES, &pages);
    if (pages < 1) pages = 1;
    int animated = pages > 1 && (!strcmp(source, "gif") || !strcmp(source, "webp"));
    int frames_total = animated ? pages : 1;
    /* OSS 源图上限：单边 30000（TIFF 16384、AVIF 10000），总像素 2.5 亿。 */
    int side = !strcmp(source, "tiff") ? 16384 : !strcmp(source, "avif") ? 10000 : 30000;
    if (width < 1 || height < 1 || width > side || height > side) return fail(5);
    if ((uint64_t)width * height * frames_total > MAX_SOURCE_PIXELS) return fail(12);
    if (!strcmp(kind, "info")) {
        int status = write_info(header, pages, output);
        if (!status) printf("info\n");
        return status;
    }
    if (!strcmp(kind, "hue")) return write_hue(input);
    int source_alpha = vips_image_hasalpha(header);
    int orientation = 1;
    if (vips_image_get_typeof(header, VIPS_META_ORIENTATION)) vips_image_get_int(header, VIPS_META_ORIENTATION, &orientation);
    g_object_unref(header);

    /* 只有全是 resize/crop 且输出 GIF/WebP 时动图才保持多帧；否则只解码第一帧。 */
    int keep = animated && (!strcmp(format, "gif") || !strcmp(format, "webp"));
    for (int i = 0; i < op_count && keep;) {
        if (!strcmp(ops[i], "resize")) i += 9;
        else if (!strcmp(ops[i], "crop")) i += 6;
        else keep = 0;
    }
    char path[4096];
    snprintf(path, sizeof(path), "%s%s", input, keep ? "[n=-1]" : "");

    Frames f = {NULL, 1};
    VipsImage *strip = NULL;
    int op = 0;
    /* 第一个操作是 resize 时直接用 vips_thumbnail：解码阶段就缩小（JPEG/WebP shrink-on-load），
     * 大照片出缩略图不必先完整解码。转正由 thumbnail 一并处理。 */
    if (op_count >= 9 && !strcmp(ops[0], "resize")) {
        Resize r = {ops[1], atoi(ops[2]), atoi(ops[3]), atoi(ops[4]), atoi(ops[5]), atoi(ops[6]), atoi(ops[7]), {0, 0, 0}};
        unsigned int rgb = (unsigned int)strtoul(ops[8], NULL, 16);
        r.color[0] = (rgb >> 16) & 255; r.color[1] = (rgb >> 8) & 255; r.color[2] = rgb & 255;
        int W = width, H = height;
        if (orient && !keep && orientation >= 5 && orientation <= 8) { W = height; H = width; }
        Geometry g; geometry(W, H, r, &g);
        if (!g.unchanged) {
            if (too_large(g.tw, g.th) || too_large(g.bw, g.bh)) return fail(8);
            if (vips_thumbnail(path, &strip, g.tw, "height", g.th, "size", VIPS_SIZE_FORCE,
                "no_rotate", !orient || keep, "fail_on", VIPS_FAIL_ON_ERROR, NULL)) return fail(4);
            op = 9;
            /* thumbnail 只做了缩放；fill/pad 的居中裁剪与填充在拆帧后逐帧补上。 */
            if (g.post) {
                int ph = vips_image_get_page_height(strip), n = keep ? vips_image_get_height(strip) / ph : 1;
                f.frame = g_new0(VipsImage *, n); f.count = n;
                for (int i = 0; i < n; i++) {
                    if (vips_extract_area(strip, &f.frame[i], 0, i * ph, vips_image_get_width(strip), ph, NULL)) return fail(4);
                    if (finish_resize(&f.frame[i], &g, &r)) return fail(4);
                }
                g_object_unref(strip); strip = NULL;
            }
        }
    }
    if (!f.frame) {
        if (!strip) {
            strip = vips_image_new_from_file(path, "fail_on", VIPS_FAIL_ON_ERROR, NULL);
            if (!strip) return fail(4);
            /* auto-orient 默认开启，在本段所有操作之前转正（动图没有方向信息）。 */
            if (orient && !keep) {
                VipsImage *rotated = NULL;
                if (vips_autorot(strip, &rotated, NULL)) return fail(4);
                g_object_unref(strip); strip = rotated;
            }
        }
        int ph = keep ? vips_image_get_page_height(strip) : vips_image_get_height(strip);
        int n = keep ? vips_image_get_height(strip) / ph : 1;
        f.frame = g_new0(VipsImage *, n); f.count = n;
        if (n == 1) { f.frame[0] = strip; strip = NULL; }
        else {
            for (int i = 0; i < n; i++)
                if (vips_extract_area(strip, &f.frame[i], 0, i * ph, vips_image_get_width(strip), ph, NULL)) return fail(4);
            g_object_unref(strip); strip = NULL;
        }
    }

    /* 按串里的先后执行其余操作。 */
    while (op < op_count) {
        const char *name = ops[op];
        int W = vips_image_get_width(f.frame[0]), H = vips_image_get_height(f.frame[0]);
        VipsImage *out = NULL;
        if (!strcmp(name, "resize")) {
            if (op + 9 > op_count) return 2;
            Resize r = {ops[op + 1], atoi(ops[op + 2]), atoi(ops[op + 3]), atoi(ops[op + 4]), atoi(ops[op + 5]), atoi(ops[op + 6]), atoi(ops[op + 7]), {0, 0, 0}};
            unsigned int rgb = (unsigned int)strtoul(ops[op + 8], NULL, 16);
            r.color[0] = (rgb >> 16) & 255; r.color[1] = (rgb >> 8) & 255; r.color[2] = rgb & 255;
            op += 9;
            Geometry g; geometry(W, H, r, &g);
            if (g.unchanged) continue;
            if (too_large(g.tw, g.th) || too_large(g.bw, g.bh)) return fail(8);
            for (int i = 0; i < f.count; i++) {
                if (vips_thumbnail_image(f.frame[i], &out, g.tw, "height", g.th, "size", VIPS_SIZE_FORCE, "no_rotate", TRUE, NULL)) return fail(4);
                replace(&f, i, out);
                if (finish_resize(&f.frame[i], &g, &r)) return fail(4);
            }
        } else if (!strcmp(name, "crop")) {
            if (op + 6 > op_count) return 2;
            int cw = atoi(ops[op + 1]), ch = atoi(ops[op + 2]);
            long long x = atoll(ops[op + 3]), y = atoll(ops[op + 4]);
            const char *gravity = ops[op + 5];
            op += 6;
            /* 宽高为 0 或超出图片时到图片边界；锚点按 OSS 九宫格表计算后加上偏移。 */
            if (cw == 0 || cw > W) cw = W;
            if (ch == 0 || ch > H) ch = H;
            long long ax = 0, ay = 0;
            if (!strcmp(gravity, "north") || !strcmp(gravity, "center") || !strcmp(gravity, "south")) ax = W / 2 - cw / 2;
            if (!strcmp(gravity, "ne") || !strcmp(gravity, "east") || !strcmp(gravity, "se")) ax = W - cw;
            if (!strcmp(gravity, "west") || !strcmp(gravity, "center") || !strcmp(gravity, "east")) ay = H / 2 - ch / 2;
            if (!strcmp(gravity, "sw") || !strcmp(gravity, "south") || !strcmp(gravity, "se")) ay = H - ch;
            long long x0 = ax + x, y0 = ay + y;
            if (x0 < 0) x0 = 0;
            if (y0 < 0) y0 = 0;
            if (x0 >= W || y0 >= H) return fail(11);
            if (cw > W - x0) cw = (int)(W - x0);
            if (ch > H - y0) ch = (int)(H - y0);
            for (int i = 0; i < f.count; i++) {
                if (vips_extract_area(f.frame[i], &out, (int)x0, (int)y0, cw, ch, NULL)) return fail(4);
                replace(&f, i, out);
            }
        } else if (!strcmp(name, "rotate")) {
            if (op + 2 > op_count) return 2;
            int angle = atoi(ops[op + 1]) % 360;
            op += 2;
            first_frame_only(&f);
            if (!angle) continue;
            if (W > MAX_ROTATE_SIDE || H > MAX_ROTATE_SIDE) return fail(7);
            if (angle % 90 == 0) {
                VipsAngle a = angle == 90 ? VIPS_ANGLE_D90 : angle == 180 ? VIPS_ANGLE_D180 : VIPS_ANGLE_D270;
                if (vips_rot(f.frame[0], &out, a, NULL)) return fail(4);
            } else {
                /* 任意角度：画布扩成外接矩形，空出来的角填白色（有透明通道时填透明），与 OSS 一致。 */
                if (to_srgb8(&f.frame[0])) return fail(4);
                int alpha = vips_image_hasalpha(f.frame[0]);
                VipsArrayDouble *background = alpha ? vips_array_double_newv(4, 255.0, 255.0, 255.0, 0.0)
                                                    : vips_array_double_newv(3, 255.0, 255.0, 255.0);
                int status = vips_rotate(f.frame[0], &out, (double)angle, "background", background, NULL);
                vips_area_unref((VipsArea *)background);
                if (status) return fail(4);
                /* OSS 的外接矩形取 floor(W|cos|+H|sin|)（400×300 转 45° 得 494×494）；
                 * libvips 向上取整，居中裁掉多出的一像素。 */
                double rad = angle * M_PI / 180.0;
                int ow = (int)floor(W * fabs(cos(rad)) + H * fabs(sin(rad)) + 1e-9);
                int oh = (int)floor(W * fabs(sin(rad)) + H * fabs(cos(rad)) + 1e-9);
                int rw = vips_image_get_width(out), rh = vips_image_get_height(out);
                if (ow >= 1 && oh >= 1 && ow <= rw && oh <= rh && (ow != rw || oh != rh)) {
                    VipsImage *trimmed = NULL;
                    if (vips_extract_area(out, &trimmed, (rw - ow) / 2, (rh - oh) / 2, ow, oh, NULL)) return fail(4);
                    g_object_unref(out); out = trimmed;
                }
            }
            replace(&f, 0, out);
        } else if (!strcmp(name, "circle") || !strcmp(name, "rounded")) {
            if (op + 2 > op_count) return 2;
            int circle = !strcmp(name, "circle"), r = atoi(ops[op + 1]);
            op += 2;
            first_frame_only(&f);
            if (to_srgb8(&f.frame[0])) return fail(4);
            int shortest = W < H ? W : H;
            VipsImage *mask = NULL;
            if (circle) {
                /* OSS：r 超过短边一半时 r=(短边-1)/2，结果边长 2r+1，以原图中心裁取。 */
                if (r > (shortest - 1) / 2) r = (shortest - 1) / 2;
                int d = 2 * r + 1;
                if (vips_extract_area(f.frame[0], &out, (W - d) / 2, (H - d) / 2, d, d, NULL)) return fail(4);
                replace(&f, 0, out);
                mask = blank_mask(d, d);
                double ink = 255;
                if (!mask || vips_draw_circle(mask, &ink, 1, r, r, r, "fill", TRUE, NULL)) return fail(4);
            } else {
                /* OSS：r 超过最大内切圆半径时取短边一半。 */
                if (r > shortest / 2) r = shortest / 2;
                mask = blank_mask(W, H);
                double ink = 255;
                if (!mask) return fail(4);
                /* 两个十字交叉的矩形加四个角圆拼出圆角矩形；宽或高为 0 的矩形不画。 */
                if ((W - 2 * r > 0 && vips_draw_rect(mask, &ink, 1, r, 0, W - 2 * r, H, "fill", TRUE, NULL)) ||
                    (H - 2 * r > 0 && vips_draw_rect(mask, &ink, 1, 0, r, W, H - 2 * r, "fill", TRUE, NULL)) ||
                    (r > 0 && (vips_draw_circle(mask, &ink, 1, r, r, r, "fill", TRUE, NULL) ||
                        vips_draw_circle(mask, &ink, 1, W - 1 - r, r, r, "fill", TRUE, NULL) ||
                        vips_draw_circle(mask, &ink, 1, r, H - 1 - r, r, "fill", TRUE, NULL) ||
                        vips_draw_circle(mask, &ink, 1, W - 1 - r, H - 1 - r, r, "fill", TRUE, NULL)))) return fail(4);
            }
            int status = apply_mask(&f.frame[0], mask);
            g_object_unref(mask);
            if (status) return fail(4);
        } else if (!strcmp(name, "indexcrop")) {
            if (op + 4 > op_count) return 2;
            int vertical = !strcmp(ops[op + 1], "y");
            long long size = atoll(ops[op + 2]), index = atoll(ops[op + 3]);
            op += 4;
            first_frame_only(&f);
            long long length = vertical ? H : W;
            /* OSS：边长超过图片或序号超过块数时返回原图。 */
            if (size < 1 || size > length || index >= (length + size - 1) / size) continue;
            long long start = index * size, span = length - start < size ? length - start : size;
            if (vips_extract_area(f.frame[0], &out, vertical ? 0 : (int)start, vertical ? (int)start : 0,
                vertical ? W : (int)span, vertical ? (int)span : H, NULL)) return fail(4);
            replace(&f, 0, out);
        } else if (!strcmp(name, "blur")) {
            if (op + 3 > op_count) return 2;
            double r = atof(ops[op + 1]), s = atof(ops[op + 2]);
            op += 3;
            first_frame_only(&f);
            /* 高斯核在半径 r 处截断：min_ampl 取该处的相对幅值，与 ImageMagick 的 radius×sigma 写法一致。 */
            double min_ampl = exp(-(r * r) / (2 * s * s));
            if (min_ampl < 1e-4) min_ampl = 1e-4;
            if (min_ampl > 0.9) min_ampl = 0.9;
            if (vips_gaussblur(f.frame[0], &out, s, "min_ampl", min_ampl, NULL)) return fail(4);
            replace(&f, 0, out);
        } else if (!strcmp(name, "bright") || !strcmp(name, "contrast")) {
            if (op + 2 > op_count) return 2;
            int bright = !strcmp(name, "bright"); double v = atof(ops[op + 1]);
            op += 2;
            first_frame_only(&f);
            /* bright：整体加减 v% 的满幅；contrast：绕中灰 128 拉伸 (1+v%) 倍。效果近似 OSS，见 docs。 */
            double a = bright ? 1.0 : 1.0 + v / 100.0, b = bright ? v * 2.55 : 128.0 * (1.0 - a);
            if (linear_colour(&f.frame[0], a, b)) return fail(4);
        } else if (!strcmp(name, "sharpen")) {
            if (op + 2 > op_count) return 2;
            double v = atof(ops[op + 1]);
            op += 2;
            first_frame_only(&f);
            if (to_srgb8(&f.frame[0])) return fail(4);
            /* OSS 取值 50–399、建议 100；映射成 libvips 的锐化强度 m2=v/50（默认 2 档）。 */
            VipsImage *in = f.frame[0];
            if (vips_image_hasalpha(in)) {
                int bands = vips_image_get_bands(in);
                VipsImage *colour = NULL, *alpha = NULL, *sharp = NULL;
                int status = vips_extract_band(in, &colour, 0, "n", bands - 1, NULL) ||
                    vips_extract_band(in, &alpha, bands - 1, NULL) ||
                    vips_sharpen(colour, &sharp, "sigma", 1.0, "m2", v / 50.0, NULL) ||
                    vips_bandjoin2(sharp, alpha, &out, NULL);
                if (colour) g_object_unref(colour);
                if (alpha) g_object_unref(alpha);
                if (sharp) g_object_unref(sharp);
                if (status) return fail(4);
            } else if (vips_sharpen(in, &out, "sigma", 1.0, "m2", v / 50.0, NULL)) return fail(4);
            replace(&f, 0, out);
        } else return 2;
    }

    /* 质量：OSS 实测规则。JPEG：缺省=源质量，q=floor(源×q%)，Q=min(源,Q)；
     * WebP/HEIC/AVIF：缺省=min(源,80)，q=floor(源×q%)，Q=Q。PNG/BMP/GIF/TIFF 忽略质量。 */
    int src_q = source_quality(input, source), q;
    int jpeg = !strcmp(format, "jpg");
    if (!strcmp(quality_mode, "rel")) q = src_q * quality_value / 100;
    else if (!strcmp(quality_mode, "abs")) q = jpeg && quality_value > src_q ? src_q : quality_value;
    else q = jpeg ? src_q : (src_q < 80 ? src_q : 80);
    if (q < 1) q = 1;

    VipsImage *image = NULL;
    if (f.count > 1) {
        int fh = vips_image_get_height(f.frame[0]);
        VipsImage *joined = NULL;
        if (vips_arrayjoin(f.frame, &joined, f.count, "across", 1, NULL)) return fail(4);
        if (vips_copy(joined, &image, NULL)) return fail(4);
        g_object_unref(joined);
        vips_image_set_int(image, VIPS_META_PAGE_HEIGHT, fh);
    } else {
        if (vips_copy(f.frame[0], &image, NULL)) return fail(4);
        /* 单帧结果去掉多页元数据，免得 GIF/WebP 保存器按旧的 page-height 切帧。 */
        vips_image_remove(image, VIPS_META_PAGE_HEIGHT);
        vips_image_remove(image, VIPS_META_N_PAGES);
    }
    for (int i = 0; i < f.count; i++) g_object_unref(f.frame[i]);
    g_free(f.frame);
    int ow = vips_image_get_width(image), oh = vips_image_get_page_height(image);

    /* 带 ICC 时先转到 sRGB 再去元数据，否则剥掉配置文件后 Display P3 等照片会偏色；转换失败保持原样。 */
    if (vips_image_get_typeof(image, VIPS_META_ICC_NAME)) {
        VipsImage *srgb = NULL;
        if (!vips_icc_transform(image, &srgb, "srgb", "embedded", TRUE, NULL)) { g_object_unref(image); image = srgb; }
        else vips_error_clear();
    }
    /* JPEG 不支持透明：白底合成；OSS 规定带透明的 HEIC 不能存成 JPEG。 */
    if (jpeg && vips_image_hasalpha(image)) {
        if (!strcmp(source, "heic") && source_alpha) return fail(6);
        VipsImage *flat = NULL; VipsArrayDouble *white = vips_array_double_newv(3, 255.0, 255.0, 255.0);
        int status = vips_flatten(image, &flat, "background", white, NULL);
        vips_area_unref((VipsArea *)white);
        if (status) return fail(4);
        g_object_unref(image); image = flat;
    }
    if (!strcmp(format, "webp") && (ow > MAX_WEBP_SIDE || oh > MAX_WEBP_SIDE)) return fail(9);
    if (!strcmp(format, "heic") && (ow > MAX_HEIC_SIDE || oh > MAX_HEIC_SIDE)) return fail(10);
    if (!strcmp(format, "avif") && (ow > MAX_AVIF_WIDTH || (uint64_t)ow * oh > MAX_AVIF_PIXELS)) return fail(10);

    int status = -1;
    if (jpeg) status = vips_jpegsave(image, output, "Q", q, "strip", TRUE, "optimize_coding", TRUE, "interlace", interlace, NULL);
    else if (!strcmp(format, "png")) status = vips_pngsave(image, output, "compression", 6, "strip", TRUE, NULL);
    else if (!strcmp(format, "webp")) status = vips_webpsave(image, output, "Q", q, "effort", 3, "strip", TRUE, NULL);
    else if (!strcmp(format, "gif")) status = vips_gifsave(image, output, "effort", 3, "strip", TRUE, NULL);
    else if (!strcmp(format, "tiff")) status = vips_tiffsave(image, output, "compression", VIPS_FOREIGN_TIFF_COMPRESSION_DEFLATE, "strip", TRUE, NULL);
    else if (!strcmp(format, "bmp")) status = vips_magicksave(image, output, "format", "BMP", "strip", TRUE, NULL);
    else if (!strcmp(format, "heic") || !strcmp(format, "avif")) status = save_heif(image, output, q, !strcmp(format, "avif"));
    if (status) return fail(4);
    printf("image %d %d %s\n", ow, oh, format);
    g_object_unref(image);
    vips_shutdown();
    return 0;
}
