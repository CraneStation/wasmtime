#include <setjmp.h>

// Note that `sigsetjmp` and `siglongjmp` are used here where possible to
// explicitly pass a 0 argument to `sigsetjmp` that we don't need to preserve
// the process signal mask. This should make this call a bit faster b/c it
// doesn't need to touch the kernel signal handling routines.
#ifdef CFG_TARGET_OS_windows
#define platform_setjmp(buf) setjmp(buf)
#define platform_longjmp(buf, arg) longjmp(buf, arg)
#define platform_jmp_buf jmp_buf
#else
#define platform_setjmp(buf) sigsetjmp(buf, 0)
#define platform_longjmp(buf, arg) siglongjmp(buf, arg)
#define platform_jmp_buf sigjmp_buf
#endif

int wasmtime_setjmp(
    void **buf_storage,
    void (*body)(void*, void*),
    void *payload,
    void *callee) {
  platform_jmp_buf buf;
  if (platform_setjmp(buf) != 0) {
    return 0;
  }
  *buf_storage = &buf;
  body(payload, callee);
  return 1;
}

void wasmtime_longjmp(void *JmpBuf) {
  platform_jmp_buf *buf = (platform_jmp_buf*) JmpBuf;
  platform_longjmp(*buf, 1);
}

// On OpenBSD, the `libc` crate does not have the appropriate definitions to get
// the faulting PC from a signal handlers's stack frame, so we provide a C
// helper here.

#ifdef __OpenBSD__

#ifndef __x86_64__
// In theory, a helper here should be all that is necessary for OpenBSD/aarch64,
// but we haven't added that yet due to lack of a test system.
#error "On OpenBSD, only x86-64 is supported."
#endif  // __x86_64__

#include <stdint.h>
#include <sys/signal.h>

const uint8_t *GetPCFromSignalContext(void *cx) {
	ucontext_t *uc = (ucontext_t *)cx;
	return (const uint8_t *)uc->sc_rip;
}

#endif  // __OpenBSD__
