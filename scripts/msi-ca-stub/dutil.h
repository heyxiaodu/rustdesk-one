/* msi-ca-stub/dutil.h - LOCAL SYNTAX GATE STUB. NOT WiX, NOT SHIPPED, NOT LINKED.
 *
 * Stand-in for WiX Toolset 4.0.5 `dutil.h` (nupkg WixToolset.DUtil.4.0.5). Only the
 * symbols res/msi/CustomActions/** uses are declared; see scripts/msi-ca-gate.sh.
 *
 * `ExitOnFailure` deliberately ends in `goto LExit`, exactly like the real macro: that
 * jump is what makes MSVC emit C2362 ("initialization of X is skipped by 'goto LExit'")
 * for a local declared with an initializer below an ExitOnFailure. Reproducing the macro
 * faithfully is the whole point of the gate, so do not "clean this up".
 *
 * The real macro relies on MSVC's permissive handling of an empty __VA_ARGS__; the
 * GNU/Clang `##__VA_ARGS__` form below is the portable spelling of the same thing and is
 * why scripts/msi-ca-gate.sh passes -Wno-gnu-zero-variadic-macro-arguments.
 */
#pragma once
#include <windows.h>

#ifdef __cplusplus
extern "C" {
#endif
void Dutil_ReleaseStr(LPWSTR pwz);
#ifdef __cplusplus
}
#endif

#define ReleaseStr(x) if (x) { Dutil_ReleaseStr(x); (x) = NULL; }

#define ExitOnFailure(x, s, ...) if (FAILED(x)) { WcaLog(LOGMSG_STANDARD, s, ##__VA_ARGS__); goto LExit; }
