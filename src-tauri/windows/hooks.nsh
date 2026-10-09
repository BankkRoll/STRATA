; Strata installer hooks, included by Tauri's NSIS template (bundle > windows >
; nsis > installerHooks). The template already handles shortcuts, the HKCU Run
; autostart value and the opt-in "Delete the application data" checkbox, which
; removes %APPDATA%\<identifier> and %LOCALAPPDATA%\<identifier>. These hooks add
; the pieces the template doesn't know about: the elevated helper (service mode),
; the ETW session and the extra autostart bookkeeping.
;
; Every step is best effort. A missing service, session or value must never
; block an install or an uninstall, so exit codes are popped and ignored.

; IMPORTANT: contract with the strata-helper track. The helper must register its
; service under this name and support `--uninstall-service`.
!define STRATA_HELPER_EXE "strata-helper.exe"
!define STRATA_SERVICE_NAME "StrataHelper"
!define STRATA_ETW_SESSION "Strata-FileActivity"

!macro STRATA_STOP_HELPER
  ; NOTE: service mode starts the helper on demand, so it may be running while
  ; we replace or delete its binary. Stop it before files are touched.
  nsExec::Exec '"$SYSDIR\sc.exe" stop "${STRATA_SERVICE_NAME}"'
  Pop $0
  ${If} ${FileExists} "$INSTDIR\${STRATA_HELPER_EXE}"
    ; An on-demand helper (UAC-launched) can still hold the file open; reuse the
    ; template's Restart Manager prompt so the user sees one consistent dialog.
    !insertmacro CheckIfAppIsRunning "$INSTDIR\${STRATA_HELPER_EXE}" "Strata helper"
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro STRATA_STOP_HELPER
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; NOTE: an in-place update never runs the uninstaller with /UPDATE today, but
  ; if it ever does, keep the user's service-mode choice across the update.
  ${If} $UpdateMode <> 1
    DetailPrint "Removing the Strata helper service"
    ${If} ${FileExists} "$INSTDIR\${STRATA_HELPER_EXE}"
      nsExec::Exec '"$INSTDIR\${STRATA_HELPER_EXE}" --uninstall-service'
      Pop $0
    ${EndIf}
    ; Fallback for a helper too old or too broken to remove itself.
    nsExec::Exec '"$SYSDIR\sc.exe" stop "${STRATA_SERVICE_NAME}"'
    Pop $0
    nsExec::Exec '"$SYSDIR\sc.exe" delete "${STRATA_SERVICE_NAME}"'
    Pop $0

    DetailPrint "Stopping the Strata activity trace session"
    nsExec::Exec '"$SYSDIR\logman.exe" stop "${STRATA_ETW_SESSION}" -ets'
    Pop $0

    ; The template deletes HKCU\...\Run\${PRODUCTNAME}. Explorer also keeps an
    ; enabled/disabled flag for it, and a per-machine Run value can exist if an
    ; admin enabled autostart for everyone.
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "${PRODUCTNAME}"
    DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}"
    DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "${PRODUCTNAME}"
  ${EndIf}

  !insertmacro STRATA_STOP_HELPER
!macroend
