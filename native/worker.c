/* 编解码只交给 libvips 及其原生 codec。这个一次性进程不持有 URL/凭据，
 * 只读父进程创建的私有输入文件；资源限制在加载第三方图片前生效。
 * 超时/内存/文件上限由内核终止，父进程回收退出状态和临时目录。 */
#include <vips/vips.h>
#include <libheif/heif.h>
#include <wand/MagickWand.h>
#include <sys/resource.h>
#include <unistd.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
/* libvips 8.14 未暴露 HEIF 编码线程配置。直接调用同一 libheif codec，
 * 明确约束 x265/AOM 的线程，避免按宿主机核数分配线程池导致容器内存耗尽。
 * 只拷贝已缩小、已校验的像素；编解码算法仍完全由成熟库负责。 */
static int save_heif(VipsImage *image, const char *path, int quality, int avif) {
    VipsImage *rgb=NULL,*pixels=NULL;
    struct heif_context *context=NULL;
    struct heif_encoder *encoder=NULL;
    struct heif_image *frame=NULL;
    struct heif_image_handle *handle=NULL;
    struct heif_encoding_options *options=NULL;
    void *data=NULL; size_t length=0; int status=-1;
    if(vips_colourspace(image,&rgb,VIPS_INTERPRETATION_sRGB,NULL) ||
       vips_cast(rgb,&pixels,VIPS_FORMAT_UCHAR,"shift",TRUE,NULL)) goto done;
    int width=vips_image_get_width(pixels),height=vips_image_get_height(pixels);
    int bands=vips_image_get_bands(pixels),alpha=vips_image_hasalpha(pixels);
    if(bands!=(alpha?4:3)) goto done;
    data=vips_image_write_to_memory(pixels,&length);
    if(!data || length!=(size_t)width*height*bands) goto done;
    context=heif_context_alloc(); if(!context) goto done;
    if(heif_context_get_encoder_for_format(context,avif?heif_compression_AV1:heif_compression_HEVC,&encoder).code) goto done;
    if(heif_encoder_set_lossy_quality(encoder,quality).code) goto done;
    if(avif) {
        if(heif_encoder_set_parameter_integer(encoder,"threads",1).code ||
           heif_encoder_set_parameter_integer(encoder,"speed",6).code) goto done;
    } else {
        if(heif_encoder_set_parameter_string(encoder,"x265:pools","none").code ||
           heif_encoder_set_parameter_string(encoder,"x265:frame-threads","1").code ||
           heif_encoder_set_parameter_string(encoder,"preset","fast").code) goto done;
    }
    if(heif_image_create(width,height,heif_colorspace_RGB,alpha?heif_chroma_interleaved_RGBA:heif_chroma_interleaved_RGB,&frame).code ||
       heif_image_add_plane(frame,heif_channel_interleaved,width,height,8).code) goto done;
    int stride=0; uint8_t *plane=heif_image_get_plane(frame,heif_channel_interleaved,&stride);
    if(!plane || stride<width*bands) goto done;
    for(int row=0;row<height;row++) memcpy(plane+(size_t)row*stride,(uint8_t*)data+(size_t)row*width*bands,(size_t)width*bands);
    options=heif_encoding_options_alloc(); if(!options) goto done;
    options->save_alpha_channel=alpha;
    if(heif_context_encode_image(context,frame,encoder,options,&handle).code ||
       heif_context_write_to_file(context,path).code) goto done;
    status=0;
done:
    if(options) heif_encoding_options_free(options);
    if(handle) heif_image_handle_release(handle);
    if(frame) heif_image_release(frame);
    if(encoder) heif_encoder_release(encoder);
    if(context) heif_context_free(context);
    g_free(data);
    if(pixels) g_object_unref(pixels);
    if(rgb) g_object_unref(rgb);
    return status;
}
static int fail(int code) { vips_error_clear(); return code; }
int main(int argc, char **argv) {
    if (argc != 10) return 2;
    struct rlimit memory = {768UL*1024*1024,768UL*1024*1024};
    struct rlimit cpu = {25,25}, file = {20UL*1024*1024,20UL*1024*1024};
    if(setrlimit(RLIMIT_AS,&memory)||setrlimit(RLIMIT_CPU,&cpu)||setrlimit(RLIMIT_FSIZE,&file)) return 3;
    alarm(30);
    if(VIPS_INIT(argv[0])) return 3;
    vips_concurrency_set(1); vips_cache_set_max(0);
    const char *input=argv[1], *output=argv[2], *source=argv[8];
    const char *format=argv[7];
    int w=atoi(argv[3]),h=atoi(argv[4]),q=atoi(argv[5]),r=atoi(argv[6]);
    uint64_t budget=strtoull(argv[9],NULL,10);
    /* 与 OSS 一致：f=gif 只有 GIF 来源才输出 GIF，其他来源保留源格式。 */
    if(!strcmp(format,"source") || (!strcmp(format,"gif") && strcmp(source,"gif"))) format=source;
    int animated_source=!strcmp(source,"gif")||!strcmp(source,"webp");
    VipsImage *header=vips_image_new_from_file(input,"access",VIPS_ACCESS_SEQUENTIAL,"fail_on",VIPS_FAIL_ON_ERROR,NULL);
    if(!header) return fail(4);
    int iw=vips_image_get_width(header),ih=vips_image_get_height(header),frames=1,orientation=1;
    if(animated_source && vips_image_get_typeof(header,"n-pages")) vips_image_get_int(header,"n-pages",&frames);
    if(vips_image_get_typeof(header,VIPS_META_ORIENTATION)) vips_image_get_int(header,VIPS_META_ORIENTATION,&orientation);
    if(iw<1||ih<1||iw>30000||ih>30000||frames<1 || (uint64_t)iw*ih*frames>budget) return fail(5);
    if(orientation>=5 && orientation<=8) {int tmp=iw;iw=ih;ih=tmp;}
    /* OSS 默认 limit=1：目标任一边需要放大时返回原尺寸。公开 File 没有 limit 参数。 */
    if(w>iw || h>ih) {
        if(r==1) { double scale=fmin(1.0,fmin((double)w/iw,(double)h/ih)); w=(int)round(iw*scale);h=(int)round(ih*scale); }
        else { w=iw;h=ih; }
    }
    if(w<1) w=1;
    if(h<1) h=1;
    int keep_frames=frames>1 && (!strcmp(format,"gif")||!strcmp(format,"webp"));
    char path[4096]; snprintf(path,sizeof(path),"%s%s",input,keep_frames?"[n=-1]":"");
    VipsImage *image=NULL;
    if(vips_thumbnail(path,&image,w,"height",h,"size",r==0?VIPS_SIZE_FORCE:VIPS_SIZE_DOWN,
        "crop",r==2?VIPS_INTERESTING_CENTRE:VIPS_INTERESTING_NONE,"fail_on",VIPS_FAIL_ON_ERROR,NULL)) return fail(4);
    int ow=vips_image_get_width(image),oh=vips_image_get_page_height(image);
    if((uint64_t)ow*vips_image_get_height(image)>budget) return fail(5);
    /* JPEG 的 q 是相对源质量；采用成熟 JPEG 量化表估计器，不手写 codec。 */
    if(!strcmp(source,"jpeg")) {
        MagickWandGenesis(); MagickWand *wand=NewMagickWand();
        if(MagickPingImage(wand,input)) {size_t quality=MagickGetImageCompressionQuality(wand);if(quality>0 && quality<=100) q=(int)round(q*quality/100.0);}
        DestroyMagickWand(wand);MagickWandTerminus();if(q<1)q=1;
    }
    /* JPEG 不支持透明，白底合成；HEIC 透明转 JPEG 遵循 OSS 的拒绝规则。 */
    if(!strcmp(format,"jpeg") && vips_image_hasalpha(image)) {
        if(!strcmp(source,"heic")) return fail(6);
        VipsImage *flat=NULL; VipsArrayDouble *white=vips_array_double_newv(3,255.0,255.0,255.0);
        int result=vips_flatten(image,&flat,"background",white,NULL);vips_area_unref((VipsArea*)white);
        if(result) return fail(4);
        g_object_unref(image);image=flat;
    }
    int status=-1;const char *mime=NULL;
    if(!strcmp(format,"jpeg")) {status=vips_jpegsave(image,output,"Q",q,"strip",TRUE,"optimize_coding",TRUE,NULL);mime="image/jpeg";}
    else if(!strcmp(format,"png")) {status=vips_pngsave(image,output,"compression",6,"strip",TRUE,NULL);mime="image/png";}
    else if(!strcmp(format,"webp")) {status=vips_webpsave(image,output,"Q",q,"effort",3,"strip",TRUE,NULL);mime="image/webp";}
    else if(!strcmp(format,"gif")) {status=vips_gifsave(image,output,"effort",3,"strip",TRUE,NULL);mime="image/gif";}
    else if(!strcmp(format,"tiff")) {status=vips_tiffsave(image,output,"compression",VIPS_FOREIGN_TIFF_COMPRESSION_DEFLATE,"strip",TRUE,NULL);mime="image/tiff";}
    else if(!strcmp(format,"bmp")) {status=vips_magicksave(image,output,"format","BMP","strip",TRUE,NULL);mime="image/bmp";}
    else if(!strcmp(format,"heic")||!strcmp(format,"avif")) {
        int avif=!strcmp(format,"avif");
        status=save_heif(image,output,q,avif);
        mime=avif?"image/avif":"image/heic";
    }
    if(status) return fail(4);
    printf("%d %d %s\n",ow,oh,mime);
    g_object_unref(image);g_object_unref(header);vips_shutdown();return 0;
}
