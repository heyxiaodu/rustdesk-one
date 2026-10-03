/* msi-ca-stub/wcautil.h - LOCAL SYNTAX GATE STUB. NOT WiX, NOT SHIPPED, NOT LINKED.
 *
 * Stand-in for WiX Toolset 4.0.5 `wcautil.h` (nupkg WixToolset.WcaUtil.4.0.5), which
 * exists locally only after `nuget restore`. Declares exactly what
 * res/msi/CustomActions/** uses; see scripts/msi-ca-gate.sh for the rationale.
 *
 * Keep in sync when the project starts using another Wca* helper: this stub is a
 * compile-time surface only, so an unlisted symbol fails the gate loudly.
 */
#pragma once
#include <windows.h>
#include <msiquery.h>
#include "dutil.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef enum WCA_MSG_LEVEL {
    LOGMSG_NONE = 0,
    LOGMSG_STANDARD = 1,
    LOGMSG_TRACEONLY = 2,
    LOGMSG_VERBOSE = 3,
    LOGMSG_EXTRA = 4
} WCA_MSG_LEVEL;

HRESULT WcaInitialize(MSIHANDLE hInstall, const char* szComponent);
HRESULT WcaFinalize(HRESULT hr);
HRESULT WcaGlobalInitialize(HINSTANCE hInst);
void    WcaGlobalFinalize(void);
HRESULT WcaGetProperty(LPCWSTR wzProperty, LPWSTR* ppwzValue);
HRESULT WcaReadStringFromCaData(LPWSTR* ppwzData, LPWSTR* ppwzOut);
void    WcaLog(WCA_MSG_LEVEL level, const char* fmt, ...);

#ifdef __cplusplus
}
#endif
