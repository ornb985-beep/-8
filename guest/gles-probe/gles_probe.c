// apex_gles_probe: render with the device's GLES driver (whatever
// ro.hardware.egl selects) into a 256x256 pbuffer and read the pixels back:
// clear to red, draw a green triangle, verify both. Results to /dev/kmsg.
#include <EGL/egl.h>
#include <GLES2/gl2.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int kmsg = -1;
static void say(const char *fmt, ...) {
    char buf[512];
    va_list ap;
    va_start(ap, fmt);
    int n = snprintf(buf, sizeof buf, "<5>APEX-GLES: ");
    n += vsnprintf(buf + n, sizeof buf - (size_t)n, fmt, ap);
    va_end(ap);
    if (kmsg >= 0) write(kmsg, buf, (size_t)n);
    fprintf(stderr, "%s\n", buf + 3);
}

static GLuint shader(GLenum type, const char *src) {
    GLuint s = glCreateShader(type);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    return s;
}

int main(void) {
    kmsg = open("/dev/kmsg", O_WRONLY | O_CLOEXEC);
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    EGLint maj = 0, min = 0;
    if (!eglInitialize(dpy, &maj, &min)) { say("FAIL eglInitialize 0x%x", eglGetError()); return 1; }
    say("EGL %d.%d vendor '%s' version '%s'", maj, min, eglQueryString(dpy, EGL_VENDOR), eglQueryString(dpy, EGL_VERSION));
    const EGLint cfg_attr[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                               EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8, EGL_NONE};
    EGLConfig cfg;
    EGLint n = 0;
    if (!eglChooseConfig(dpy, cfg_attr, &cfg, 1, &n) || n < 1) { say("FAIL eglChooseConfig"); return 1; }
    const EGLint pb[] = {EGL_WIDTH, 256, EGL_HEIGHT, 256, EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, pb);
    const EGLint ctx_attr[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attr);
    if (surf == EGL_NO_SURFACE || ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, surf, surf, ctx)) {
        say("FAIL surface/context 0x%x", eglGetError());
        return 1;
    }
    say("GL_VENDOR '%s' GL_RENDERER '%s' GL_VERSION '%s'", glGetString(GL_VENDOR), glGetString(GL_RENDERER),
        glGetString(GL_VERSION));
    glViewport(0, 0, 256, 256);
    glClearColor(1.f, 0.f, 0.f, 1.f);
    glClear(GL_COLOR_BUFFER_BIT);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, shader(GL_VERTEX_SHADER, "attribute vec2 p; void main(){ gl_Position = vec4(p, 0.0, 1.0); }"));
    glAttachShader(prog, shader(GL_FRAGMENT_SHADER, "precision mediump float; void main(){ gl_FragColor = vec4(0.0, 1.0, 0.0, 1.0); }"));
    glBindAttribLocation(prog, 0, "p");
    glLinkProgram(prog);
    glUseProgram(prog);
    const GLfloat tri[] = {-1.f, -1.f, 1.f, -1.f, -1.f, 1.f};  // lower-left half
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, tri);
    glEnableVertexAttribArray(0);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    static unsigned char px[256 * 256 * 4];
    glReadPixels(0, 0, 256, 256, GL_RGBA, GL_UNSIGNED_BYTE, px);
    GLenum err = glGetError();
    unsigned red = 0, green = 0, other = 0;
    for (int i = 0; i < 256 * 256; i++) {
        const unsigned char *p = px + i * 4;
        if (p[0] > 200 && p[1] < 50 && p[2] < 50) red++;
        else if (p[0] < 50 && p[1] > 200 && p[2] < 50) green++;
        else other++;
    }
    const unsigned char *ll = px + (10 * 256 + 10) * 4, *ur = px + (245 * 256 + 245) * 4;
    int ok = err == GL_NO_ERROR && green > 30000 && red > 30000 && ll[1] > 200 && ur[0] > 200;
    say("%s: glReadPixels err 0x%x, red %u green %u other %u, lower-left (%u,%u,%u) upper-right (%u,%u,%u)",
        ok ? "PASS" : "FAIL", err, red, green, other, ll[0], ll[1], ll[2], ur[0], ur[1], ur[2]);
    return ok ? 0 : 1;
}
