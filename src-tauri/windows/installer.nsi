; Strata's NSIS installer template (bundle > windows > nsis > template).
;
; IMPORTANT: forked from tauri-bundler's installer.nsi as of @tauri-apps/cli
; 2.12.1. Sections, registry layout, the uninstaller and every command-line
; mode (/S, /P, /UPDATE, /NS, /R, /ARGS, /D=) are unchanged, because the
; updater and the "reinstall the previous version" rollback rely on them. Only
; the interactive pages differ:
;
;   1. One install page (install, update, reinstall or older-version install,
;      location, desktop shortcut) instead of welcome, reinstall, directory and
;      start-menu pages. Existing installs are updated in place, the same way
;      the updater does it, so settings, history and shortcuts are kept.
;   2. The progress page.
;   3. One finish page with "Launch Strata".
;
; Additional checks: Windows 10 or later, the installer's architecture
; matches the PC, a second copy of Setup is not already running, the chosen
; folder is local, outside Windows and has room.
;
; When upgrading the Tauri CLI, diff the new upstream template against
; 2.12.1 and carry over any changes outside the page code.

Unicode true
ManifestDPIAware true
; Add in `dpiAwareness` `PerMonitorV2` to manifest for Windows 10 1607+ (note this should not affect lower versions since they should be able to ignore this and pick up `dpiAware` `true` set by `ManifestDPIAware true`)
; Currently undocumented on NSIS's website but is in the Docs folder of source tree, see
; https://github.com/kichik/nsis/blob/5fc0b87b819a9eec006df4967d08e522ddd651c9/Docs/src/attributes.but#L286-L300
; https://github.com/tauri-apps/tauri/pull/10106
ManifestDPIAwareness PerMonitorV2

!if "{{compression}}" == "none"
  SetCompress off
!else
  ; Set the compression algorithm. We default to LZMA.
  SetCompressor /SOLID "{{compression}}"
!endif

; Keep above !include to stay ahead of any plugin command
; see https://github.com/tauri-apps/tauri/pull/15422#discussion_r3289239624
{{#if signed_plugins_path}}
!addplugindir "{{signed_plugins_path}}"
{{/if}}

!include MUI2.nsh
!include FileFunc.nsh
!include x64.nsh
!include WordFunc.nsh
!include "utils.nsh"
!include "FileAssociation.nsh"
!include "Win\COM.nsh"
!include "Win\Propkey.nsh"
!include "Win\RestartManager.nsh"
!include WinVer.nsh
!include nsDialogs.nsh

{{#if installer_hooks}}
!include "{{installer_hooks}}"
{{/if}}

!define WEBVIEW2APPGUID "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"

!define MANUFACTURER "{{manufacturer}}"
!define PRODUCTNAME "{{product_name}}"
!define VERSION "{{version}}"
!define VERSIONWITHBUILD "{{version_with_build}}"
!define HOMEPAGE "{{homepage}}"
!define INSTALLMODE "{{install_mode}}"
!define LICENSE "{{license}}"
!define INSTALLERICON "{{installer_icon}}"
!define SIDEBARIMAGE "{{sidebar_image}}"
!define HEADERIMAGE "{{header_image}}"
!define UNINSTALLERICON "{{uninstaller_icon}}"
!define UNINSTALLERHEADERIMAGE "{{uninstaller_header_image}}"
!define MAINBINARYNAME "{{main_binary_name}}"
!define MAINBINARYSRCPATH "{{main_binary_path}}"
!define BUNDLEID "{{bundle_id}}"
!define COPYRIGHT "{{copyright}}"
!define OUTFILE "{{out_file}}"
!define ARCH "{{arch}}"
!define ADDITIONALPLUGINSPATH "{{additional_plugins_path}}"
!define ALLOWDOWNGRADES "{{allow_downgrades}}"
!define DISPLAYLANGUAGESELECTOR "{{display_language_selector}}"
!define INSTALLWEBVIEW2MODE "{{install_webview2_mode}}"
!define WEBVIEW2INSTALLERARGS "{{webview2_installer_args}}"
!define WEBVIEW2BOOTSTRAPPERPATH "{{webview2_bootstrapper_path}}"
!define WEBVIEW2INSTALLERPATH "{{webview2_installer_path}}"
!define MINIMUMWEBVIEW2VERSION "{{minimum_webview2_version}}"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}"
!define MANUKEY "Software\${MANUFACTURER}"
!define MANUPRODUCTKEY "${MANUKEY}\${PRODUCTNAME}"
!define UNINSTALLERSIGNCOMMAND "{{uninstaller_sign_cmd}}"
!define ESTIMATEDSIZE "{{estimated_size}}"
!define STARTMENUFOLDER "{{start_menu_folder}}"

Var PassiveMode
Var UpdateMode
Var NoShortcutMode
Var WixMode
Var OldMainBinaryName

; What is already installed: "none", "older" (this installer updates it),
; "same" (reinstall) or "newer" (this installer is older). Set in .onInit so
; it is valid in silent and passive mode too, where no page callback runs.
Var ExistingState
Var ExistingVersion
Var DesktopShortcut
Var HighContrast

Name "${PRODUCTNAME}"
; NOTE: a single space hides the branding line; an empty value would show the
; default "Nullsoft Install System" text instead.
BrandingText " "
OutFile "${OUTFILE}"

; The Windows UI font at its standard size, instead of the 8pt dialog font.
SetFont "Segoe UI" 9

!define STRATA_RELEASE_URL "${HOMEPAGE}/releases/tag/v${VERSION}"
!define STRATA_TEXT "1A1A1A"
!define STRATA_MUTED "5C5C5C"
!define STRATA_BG "FFFFFF"
!define STRATA_PANEL "0A0A0A"
!define STRATA_PANEL_TEXT "EDEDED"
!define STRATA_PANEL_MUTED "A3A3A3"
!define STRATA_LINK "0B57D0"
!define /ifndef SS_PATHELLIPSIS 0x00008000
!define /ifndef ERROR_ALREADY_EXISTS 183

; We don't actually use this value as default install path,
; it's just for nsis to append the product name folder in the directory selector
; https://nsis.sourceforge.io/Reference/InstallDir
!define PLACEHOLDER_INSTALL_DIR "placeholder\${PRODUCTNAME}"
InstallDir "${PLACEHOLDER_INSTALL_DIR}"

VIProductVersion "${VERSIONWITHBUILD}"
VIAddVersionKey "ProductName" "${PRODUCTNAME}"
VIAddVersionKey "FileDescription" "${PRODUCTNAME}"
VIAddVersionKey "LegalCopyright" "${COPYRIGHT}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"

# additional plugins
!addplugindir "${ADDITIONALPLUGINSPATH}"

; Uninstaller signing command
!if "${UNINSTALLERSIGNCOMMAND}" != ""
  !uninstfinalize '${UNINSTALLERSIGNCOMMAND}'
!endif

; Handle install mode, `perUser`, `perMachine` or `both`
!if "${INSTALLMODE}" == "perMachine"
  RequestExecutionLevel admin
!endif

!if "${INSTALLMODE}" == "currentUser"
  RequestExecutionLevel user
!endif

!if "${INSTALLMODE}" == "both"
  !error "Strata's installer template supports the perMachine and currentUser install modes only."
!endif
!if "${LICENSE}" != ""
  !warning "bundle > licenseFile is set, but Strata's installer has no license page."
!endif

!if "${INSTALLMODE}" == "both"
  !define MULTIUSER_MUI
  !define MULTIUSER_INSTALLMODE_INSTDIR "${PRODUCTNAME}"
  !define MULTIUSER_INSTALLMODE_COMMANDLINE
  !if "${ARCH}" == "x64"
    !define MULTIUSER_USE_PROGRAMFILES64
  !else if "${ARCH}" == "arm64"
    !define MULTIUSER_USE_PROGRAMFILES64
  !endif
  !define MULTIUSER_INSTALLMODE_DEFAULT_REGISTRY_KEY "${UNINSTKEY}"
  !define MULTIUSER_INSTALLMODE_DEFAULT_REGISTRY_VALUENAME "CurrentUser"
  !define MULTIUSER_INSTALLMODEPAGE_SHOWUSERNAME
  !define MULTIUSER_INSTALLMODE_FUNCTION RestorePreviousInstallLocation
  !define MULTIUSER_EXECUTIONLEVEL Highest
  !include MultiUser.nsh
!endif

; Installer icon
!if "${INSTALLERICON}" != ""
  !define MUI_ICON "${INSTALLERICON}"
!endif

; Installer sidebar image
!if "${SIDEBARIMAGE}" != ""
  !define MUI_WELCOMEFINISHPAGE_BITMAP "${SIDEBARIMAGE}"
!endif

; Enable header images for installer and uninstaller pages when either image is configured.
!if "${HEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE
!else if "${UNINSTALLERHEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE
!endif

; Installer header image
!if "${HEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE_BITMAP "${HEADERIMAGE}"
!endif

; Uninstaller header image
!if "${UNINSTALLERHEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE_UNBITMAP "${UNINSTALLERHEADERIMAGE}"
!endif

; Uninstaller icon
!if "${UNINSTALLERICON}" != ""
  !define MUI_UNICON "${UNINSTALLERICON}"
!endif

; Define registry key to store installer language
!define MUI_LANGDLL_REGISTRY_ROOT "HKCU"
!define MUI_LANGDLL_REGISTRY_KEY "${MANUPRODUCTKEY}"
!define MUI_LANGDLL_REGISTRY_VALUENAME "Installer Language"

; Ask before closing Setup halfway through.
!define MUI_ABORTWARNING
!define MUI_ABORTWARNING_TEXT "Quit Strata Setup? Strata won't be installed."

; Installer pages, in the order they appear
; 1. Install page: install, update, reinstall or older-version install
Page custom StrataInstallPage StrataInstallPageLeave

; 2. Start menu page: never shown, but its registry plumbing is what the
;    shortcut code and the uninstaller read
Var AppStartMenuFolder
!if "${STARTMENUFOLDER}" != ""
  !define MUI_PAGE_CUSTOMFUNCTION_PRE Skip
  !define MUI_STARTMENUPAGE_DEFAULTFOLDER "${STARTMENUFOLDER}"
!else
  !define MUI_PAGE_CUSTOMFUNCTION_PRE Skip
!endif
!insertmacro MUI_PAGE_STARTMENU Application $AppStartMenuFolder

; 3. Progress page. It moves on to the finish page by itself; "Show details"
;    still has the full log if something goes wrong.
!define MUI_PAGE_CUSTOMFUNCTION_SHOW StrataProgressShow
!define MUI_INSTFILESPAGE_ABORTHEADER_TEXT "Setup didn't finish"
!define MUI_INSTFILESPAGE_ABORTHEADER_SUBTEXT "Strata may not work until Setup runs to the end. Run it again to finish."
!insertmacro MUI_PAGE_INSTFILES

; 4. Finish page
Page custom StrataFinishPage StrataFinishPageLeave

; NOTE: declares muiPageLoadFullWindow/muiPageUnloadFullWindow, which hide the
; header band for the install and finish pages. It must come while the
; installer's pages are being declared, before any uninstaller page.
!insertmacro MUI_PAGE_FUNCTION_FULLWINDOW

Function RunMainBinary
  nsis_tauri_utils::RunAsUser "$INSTDIR\${MAINBINARYNAME}.exe" ""
FunctionEnd

; Uninstaller Pages
; 1. Confirm uninstall page
Var DeleteAppDataCheckbox
Var DeleteAppDataCheckboxState
!define /ifndef WS_EX_LAYOUTRTL         0x00400000
!define MUI_PAGE_CUSTOMFUNCTION_SHOW un.ConfirmShow
Function un.ConfirmShow ; Add add a `Delete app data` check box
  ; $1 inner dialog HWND
  ; $2 window DPI
  ; $3 style
  ; $4 x
  ; $5 y
  ; $6 width
  ; $7 height
  FindWindow $1 "#32770" "" $HWNDPARENT ; Find inner dialog
  System::Call "user32::GetDpiForWindow(p r1) i .r2"
  ${If} $(^RTL) = 1
    StrCpy $3 "${__NSD_CheckBox_EXSTYLE} | ${WS_EX_LAYOUTRTL}"
    IntOp $4 50 * $2
  ${Else}
    StrCpy $3 "${__NSD_CheckBox_EXSTYLE}"
    IntOp $4 0 * $2
  ${EndIf}
  IntOp $5 100 * $2
  IntOp $6 400 * $2
  IntOp $7 25 * $2
  IntOp $4 $4 / 96
  IntOp $5 $5 / 96
  IntOp $6 $6 / 96
  IntOp $7 $7 / 96
  System::Call 'user32::CreateWindowEx(i r3, w "${__NSD_CheckBox_CLASS}", w "$(deleteAppData)", i ${__NSD_CheckBox_STYLE}, i r4, i r5, i r6, i r7, p r1, i0, i0, i0) i .s'
  Pop $DeleteAppDataCheckbox
  SendMessage $HWNDPARENT ${WM_GETFONT} 0 0 $1
  SendMessage $DeleteAppDataCheckbox ${WM_SETFONT} $1 1
FunctionEnd
!define MUI_PAGE_CUSTOMFUNCTION_LEAVE un.ConfirmLeave
Function un.ConfirmLeave
  SendMessage $DeleteAppDataCheckbox ${BM_GETCHECK} 0 0 $DeleteAppDataCheckboxState
FunctionEnd
!define MUI_PAGE_CUSTOMFUNCTION_PRE un.SkipIfPassive
!insertmacro MUI_UNPAGE_CONFIRM

; 2. Uninstalling Page
!insertmacro MUI_UNPAGE_INSTFILES

;Languages
{{#each languages}}
!insertmacro MUI_LANGUAGE "{{this}}"
{{/each}}
!insertmacro MUI_RESERVEFILE_LANGDLL
{{#each language_files}}
  !include "{{this}}"
{{/each}}

Function .onInit
  ${GetOptions} $CMDLINE "/P" $PassiveMode
  ${IfNot} ${Errors}
    StrCpy $PassiveMode 1
  ${EndIf}

  ${GetOptions} $CMDLINE "/NS" $NoShortcutMode
  ${IfNot} ${Errors}
    StrCpy $NoShortcutMode 1
  ${EndIf}

  ${GetOptions} $CMDLINE "/UPDATE" $UpdateMode
  ${IfNot} ${Errors}
    StrCpy $UpdateMode 1
  ${EndIf}

  !if "${DISPLAYLANGUAGESELECTOR}" == "true"
    !insertmacro MUI_LANGDLL_DISPLAY
  !endif

  ; NOTE: Strata never shipped an MSI, so there is no WiX install to migrate.
  StrCpy $WixMode 0

  Call StrataCheckSystem

  !insertmacro SetContext

  ${If} $INSTDIR == "${PLACEHOLDER_INSTALL_DIR}"
    ; Set default install location
    !if "${INSTALLMODE}" == "perMachine"
      ${If} ${RunningX64}
        !if "${ARCH}" == "x64"
          StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"
        !else if "${ARCH}" == "arm64"
          StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"
        !else
          StrCpy $INSTDIR "$PROGRAMFILES\${PRODUCTNAME}"
        !endif
      ${Else}
        StrCpy $INSTDIR "$PROGRAMFILES\${PRODUCTNAME}"
      ${EndIf}
    !else if "${INSTALLMODE}" == "currentUser"
      StrCpy $INSTDIR "$LOCALAPPDATA\${PRODUCTNAME}"
    !endif

    Call RestorePreviousInstallLocation
  ${EndIf}


  !if "${INSTALLMODE}" == "both"
    !insertmacro MULTIUSER_INIT
  !endif

  Call StrataDetectExisting
FunctionEnd


Section EarlyChecks
  ; Abort silent installer if downgrades is disabled
  !if "${ALLOWDOWNGRADES}" == "false"
  ${If} ${Silent}
    ; If downgrading
    ${If} $ExistingState == "newer"
      System::Call 'kernel32::AttachConsole(i -1)i.r0'
      ${If} $0 <> 0
        System::Call 'kernel32::GetStdHandle(i -11)i.r0'
        System::call 'kernel32::SetConsoleTextAttribute(i r0, i 0x0004)' ; set red color
        FileWrite $0 "$(silentDowngrades)"
      ${EndIf}
      Abort
    ${EndIf}
  ${EndIf}
  !endif

SectionEnd

Section WebView2
  ; Check if Webview2 is already installed and skip this section
  ${If} ${RunningX64}
    ReadRegStr $4 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\${WEBVIEW2APPGUID}" "pv"
  ${Else}
    ReadRegStr $4 HKLM "SOFTWARE\Microsoft\EdgeUpdate\Clients\${WEBVIEW2APPGUID}" "pv"
  ${EndIf}
  ${If} $4 == ""
    ReadRegStr $4 HKCU "SOFTWARE\Microsoft\EdgeUpdate\Clients\${WEBVIEW2APPGUID}" "pv"
  ${EndIf}

  ${If} $4 == ""
    ; Webview2 installation
    ;
    ; Skip if updating
    ${If} $UpdateMode <> 1
      !if "${INSTALLWEBVIEW2MODE}" == "downloadBootstrapper"
        Delete "$TEMP\MicrosoftEdgeWebview2Setup.exe"
        DetailPrint "$(webview2Downloading)"
        NSISdl::download "https://go.microsoft.com/fwlink/p/?LinkId=2124703" "$TEMP\MicrosoftEdgeWebview2Setup.exe"
        Pop $0
        ${If} $0 == "success"
          DetailPrint "$(webview2DownloadSuccess)"
        ${Else}
          DetailPrint "$(webview2DownloadError)"
          Abort "$(webview2AbortError)"
        ${EndIf}
        StrCpy $6 "$TEMP\MicrosoftEdgeWebview2Setup.exe"
        Goto install_webview2
      !endif

      !if "${INSTALLWEBVIEW2MODE}" == "embedBootstrapper"
        Delete "$TEMP\MicrosoftEdgeWebview2Setup.exe"
        File "/oname=$TEMP\MicrosoftEdgeWebview2Setup.exe" "${WEBVIEW2BOOTSTRAPPERPATH}"
        DetailPrint "$(installingWebview2)"
        StrCpy $6 "$TEMP\MicrosoftEdgeWebview2Setup.exe"
        Goto install_webview2
      !endif

      !if "${INSTALLWEBVIEW2MODE}" == "offlineInstaller"
        Delete "$TEMP\MicrosoftEdgeWebView2RuntimeInstaller.exe"
        File "/oname=$TEMP\MicrosoftEdgeWebView2RuntimeInstaller.exe" "${WEBVIEW2INSTALLERPATH}"
        DetailPrint "$(installingWebview2)"
        StrCpy $6 "$TEMP\MicrosoftEdgeWebView2RuntimeInstaller.exe"
        Goto install_webview2
      !endif

      Goto webview2_done

      install_webview2:
        DetailPrint "$(installingWebview2)"
        ; $6 holds the path to the webview2 installer
        ExecWait "$6 ${WEBVIEW2INSTALLERARGS} /install" $1
        ${If} $1 = 0
          DetailPrint "$(webview2InstallSuccess)"
        ${Else}
          DetailPrint "$(webview2InstallError)"
          Abort "$(webview2AbortError)"
        ${EndIf}
      webview2_done:
    ${EndIf}
  ${Else}
    !if "${MINIMUMWEBVIEW2VERSION}" != ""
      ${VersionCompare} "${MINIMUMWEBVIEW2VERSION}" "$4" $R0
      ${If} $R0 = 1
        update_webview:
          DetailPrint "$(installingWebview2)"
          ${If} ${RunningX64}
            ReadRegStr $R1 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate" "path"
          ${Else}
            ReadRegStr $R1 HKLM "SOFTWARE\Microsoft\EdgeUpdate" "path"
          ${EndIf}
          ${If} $R1 == ""
            ReadRegStr $R1 HKCU "SOFTWARE\Microsoft\EdgeUpdate" "path"
          ${EndIf}
          ${If} $R1 != ""
            ; Chromium updater docs: https://source.chromium.org/chromium/chromium/src/+/main:docs/updater/user_manual.md
            ; Modified from "HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Microsoft EdgeWebView\ModifyPath"
            ExecWait `"$R1" /install appguid=${WEBVIEW2APPGUID}&needsadmin=true` $1
            ${If} $1 = 0
              DetailPrint "$(webview2InstallSuccess)"
            ${Else}
              MessageBox MB_ICONEXCLAMATION|MB_ABORTRETRYIGNORE "$(webview2InstallError)" IDIGNORE ignore IDRETRY update_webview
              Quit
              ignore:
            ${EndIf}
          ${EndIf}
      ${EndIf}
    !endif
  ${EndIf}
SectionEnd

Section Install
  SetOutPath $INSTDIR

  !ifmacrodef NSIS_HOOK_PREINSTALL
    !insertmacro NSIS_HOOK_PREINSTALL
  !endif

  !insertmacro CheckIfAppIsRunning "$INSTDIR\${MAINBINARYNAME}.exe" "${PRODUCTNAME}"

  ; Copy main executable
  File "${MAINBINARYSRCPATH}"

  ; Copy resources
  {{#each resources_dirs}}
    CreateDirectory "$INSTDIR\\{{this}}"
  {{/each}}
  {{#each resources}}
    File /a "/oname={{this.[1]}}" "{{no-escape @key}}"
  {{/each}}

  ; Copy external binaries
  {{#each binaries}}
    File /a "/oname={{this}}" "{{no-escape @key}}"
  {{/each}}

  ; Create file associations
  {{#each file_associations as |association| ~}}
    {{#each association.ext as |ext| ~}}
       !insertmacro APP_ASSOCIATE "{{ext}}" "{{or association.name ext}}" "{{association-description association.description ext}}" "$INSTDIR\${MAINBINARYNAME}.exe,0" "Open with ${PRODUCTNAME}" "$INSTDIR\${MAINBINARYNAME}.exe $\"%1$\""
    {{/each}}
  {{/each}}

  ; Register deep links
  {{#each deep_link_protocols as |protocol| ~}}
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}" "URL Protocol" ""
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}" "" "URL:${BUNDLEID} protocol"
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}\DefaultIcon" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\",0"
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}\shell\open\command" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\""
  {{/each}}

  ; Create uninstaller
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Save $INSTDIR in registry for future installations
  WriteRegStr SHCTX "${MANUPRODUCTKEY}" "" $INSTDIR

  !if "${INSTALLMODE}" == "both"
    ; Save install mode to be selected by default for the next installation such as updating
    ; or when uninstalling
    WriteRegStr SHCTX "${UNINSTKEY}" $MultiUser.InstallMode 1
  !endif

  ; Remove old main binary if it doesn't match new main binary name
  ReadRegStr $OldMainBinaryName SHCTX "${UNINSTKEY}" "MainBinaryName"
  ${If} $OldMainBinaryName != ""
  ${AndIf} $OldMainBinaryName != "${MAINBINARYNAME}.exe"
    Delete "$INSTDIR\$OldMainBinaryName"
  ${EndIf}

  ; Save current MAINBINARYNAME for future updates
  WriteRegStr SHCTX "${UNINSTKEY}" "MainBinaryName" "${MAINBINARYNAME}.exe"

  ; Registry information for add/remove programs
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayName" "${PRODUCTNAME}"
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayIcon" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\""
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr SHCTX "${UNINSTKEY}" "Publisher" "${MANUFACTURER}"
  WriteRegStr SHCTX "${UNINSTKEY}" "InstallLocation" "$\"$INSTDIR$\""
  WriteRegStr SHCTX "${UNINSTKEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegDWORD SHCTX "${UNINSTKEY}" "NoModify" "1"
  WriteRegDWORD SHCTX "${UNINSTKEY}" "NoRepair" "1"

  ${GetSize} "$INSTDIR" "/M=uninstall.exe /S=0K /G=0" $0 $1 $2
  IntOp $0 $0 + ${ESTIMATEDSIZE}
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD SHCTX "${UNINSTKEY}" "EstimatedSize" "$0"

  !if "${HOMEPAGE}" != ""
    WriteRegStr SHCTX "${UNINSTKEY}" "URLInfoAbout" "${HOMEPAGE}"
    WriteRegStr SHCTX "${UNINSTKEY}" "URLUpdateInfo" "${HOMEPAGE}"
    WriteRegStr SHCTX "${UNINSTKEY}" "HelpLink" "${HOMEPAGE}"
  !endif

  ; Create start menu shortcut
  !insertmacro MUI_STARTMENU_WRITE_BEGIN Application
    Call CreateOrUpdateStartMenuShortcut
  !insertmacro MUI_STARTMENU_WRITE_END

  ; Desktop shortcut: the install page's checkbox decides, silent and passive
  ; installs always create one (as upstream does)
  ${If} $PassiveMode = 1
  ${OrIf} ${Silent}
  ${OrIf} $DesktopShortcut = 1
    Call CreateOrUpdateDesktopShortcut
  ${EndIf}

  !ifmacrodef NSIS_HOOK_POSTINSTALL
    !insertmacro NSIS_HOOK_POSTINSTALL
  !endif

  ; Auto close this page for passive mode
  ${If} $PassiveMode = 1
    SetAutoClose true
  ${EndIf}
SectionEnd

Function .onInstSuccess
  ; Check for `/R` flag only in silent and passive installers because
  ; GUI installer has a toggle for the user to (re)start the app
  ${If} $PassiveMode = 1
  ${OrIf} ${Silent}
    ${GetOptions} $CMDLINE "/R" $R0
    ${IfNot} ${Errors}
      ${GetOptions} $CMDLINE "/ARGS" $R0
      nsis_tauri_utils::RunAsUser "$INSTDIR\${MAINBINARYNAME}.exe" "$R0"
    ${EndIf}
  ${EndIf}
FunctionEnd

Function un.onInit
  !insertmacro SetContext

  !if "${INSTALLMODE}" == "both"
    !insertmacro MULTIUSER_UNINIT
  !endif

  !insertmacro MUI_UNGETLANGUAGE

  ${GetOptions} $CMDLINE "/P" $PassiveMode
  ${IfNot} ${Errors}
    StrCpy $PassiveMode 1
  ${EndIf}

  ${GetOptions} $CMDLINE "/UPDATE" $UpdateMode
  ${IfNot} ${Errors}
    StrCpy $UpdateMode 1
  ${EndIf}
FunctionEnd

Section Uninstall

  !ifmacrodef NSIS_HOOK_PREUNINSTALL
    !insertmacro NSIS_HOOK_PREUNINSTALL
  !endif

  !insertmacro CheckIfAppIsRunning "$INSTDIR\${MAINBINARYNAME}.exe" "${PRODUCTNAME}"

  ; Delete the app directory and its content from disk
  ; Copy main executable
  Delete "$INSTDIR\${MAINBINARYNAME}.exe"

  ; Delete resources
  {{#each resources}}
    Delete "$INSTDIR\\{{this.[1]}}"
  {{/each}}

  ; Delete external binaries
  {{#each binaries}}
    Delete "$INSTDIR\\{{this}}"
  {{/each}}

  ; Delete app associations
  {{#each file_associations as |association| ~}}
    {{#each association.ext as |ext| ~}}
      !insertmacro APP_UNASSOCIATE "{{ext}}" "{{or association.name ext}}"
    {{/each}}
  {{/each}}

  ; Delete deep links
  {{#each deep_link_protocols as |protocol| ~}}
    ReadRegStr $R7 SHCTX "Software\Classes\\{{protocol}}\shell\open\command" ""
    ${If} $R7 == "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\""
      DeleteRegKey SHCTX "Software\Classes\\{{protocol}}"
    ${EndIf}
  {{/each}}


  ; Delete uninstaller
  Delete "$INSTDIR\uninstall.exe"

  {{#each resources_ancestors}}
  RMDir /REBOOTOK "$INSTDIR\\{{this}}"
  {{/each}}
  RMDir "$INSTDIR"

  ; Remove shortcuts if not updating
  ${If} $UpdateMode <> 1
    !insertmacro DeleteAppUserModelId

    ; Remove start menu shortcut
    !insertmacro MUI_STARTMENU_GETFOLDER Application $AppStartMenuFolder
    !insertmacro IsShortcutTarget "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    Pop $0
    ${If} $0 = 1
      !insertmacro UnpinShortcut "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk"
      Delete "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk"
      RMDir "$SMPROGRAMS\$AppStartMenuFolder"
    ${EndIf}
    !insertmacro IsShortcutTarget "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    Pop $0
    ${If} $0 = 1
      !insertmacro UnpinShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk"
      Delete "$SMPROGRAMS\${PRODUCTNAME}.lnk"
    ${EndIf}

    ; Remove desktop shortcuts
    !insertmacro IsShortcutTarget "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    Pop $0
    ${If} $0 = 1
      !insertmacro UnpinShortcut "$DESKTOP\${PRODUCTNAME}.lnk"
      Delete "$DESKTOP\${PRODUCTNAME}.lnk"
    ${EndIf}
  ${EndIf}

  ; Remove registry information for add/remove programs
  !if "${INSTALLMODE}" == "both"
    DeleteRegKey SHCTX "${UNINSTKEY}"
  !else if "${INSTALLMODE}" == "perMachine"
    DeleteRegKey HKLM "${UNINSTKEY}"
  !else
    DeleteRegKey HKCU "${UNINSTKEY}"
  !endif

  ; Removes the Autostart entry for ${PRODUCTNAME} from the HKCU Run key if it exists.
  ; This ensures the program does not launch automatically after uninstallation if it exists.
  ; If it doesn't exist, it does nothing.
  ; We do this when not updating (to preserve the registry value on updates)
  ${If} $UpdateMode <> 1
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}"
  ${EndIf}

  ; Delete app data if the checkbox is selected
  ; and if not updating
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    ; Clear the install location $INSTDIR from registry
    DeleteRegKey SHCTX "${MANUPRODUCTKEY}"
    DeleteRegKey /ifempty SHCTX "${MANUKEY}"

    ; Clear the install language from registry
    DeleteRegValue HKCU "${MANUPRODUCTKEY}" "Installer Language"
    DeleteRegKey /ifempty HKCU "${MANUPRODUCTKEY}"
    DeleteRegKey /ifempty HKCU "${MANUKEY}"

    SetShellVarContext current
    RmDir /r "$APPDATA\${BUNDLEID}"
    RmDir /r "$LOCALAPPDATA\${BUNDLEID}"
  ${EndIf}

  !ifmacrodef NSIS_HOOK_POSTUNINSTALL
    !insertmacro NSIS_HOOK_POSTUNINSTALL
  !endif

  ; Auto close if passive mode or updating
  ${If} $PassiveMode = 1
  ${OrIf} $UpdateMode = 1
    SetAutoClose true
  ${EndIf}
SectionEnd

Function RestorePreviousInstallLocation
  ReadRegStr $4 SHCTX "${MANUPRODUCTKEY}" ""
  StrCmp $4 "" +2 0
    StrCpy $INSTDIR $4
FunctionEnd

Function Skip
  Abort
FunctionEnd

Function un.SkipIfPassive
  ${IfThen} $PassiveMode = 1  ${|} Abort ${|}
FunctionEnd

Function CreateOrUpdateStartMenuShortcut
  ; We used to use product name as MAINBINARYNAME
  ; migrate old shortcuts to target the new MAINBINARYNAME
  StrCpy $R0 0

  !insertmacro IsShortcutTarget "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\$OldMainBinaryName"
  Pop $0
  ${If} $0 = 1
    !insertmacro SetShortcutTarget "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    StrCpy $R0 1
  ${EndIf}

  !insertmacro IsShortcutTarget "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\$OldMainBinaryName"
  Pop $0
  ${If} $0 = 1
    !insertmacro SetShortcutTarget "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    StrCpy $R0 1
  ${EndIf}

  ${If} $R0 = 1
    Return
  ${EndIf}

  ; Skip creating shortcut if in update mode or no shortcut mode
  ; but always create if migrating from wix
  ${If} $WixMode = 0
    ${If} $UpdateMode = 1
    ${OrIf} $NoShortcutMode = 1
      Return
    ${EndIf}
  ${EndIf}

  !if "${STARTMENUFOLDER}" != ""
    CreateDirectory "$SMPROGRAMS\$AppStartMenuFolder"
    CreateShortcut "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk"
  !else
    CreateShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\${PRODUCTNAME}.lnk"
  !endif
FunctionEnd

Function CreateOrUpdateDesktopShortcut
  ; We used to use product name as MAINBINARYNAME
  ; migrate old shortcuts to target the new MAINBINARYNAME
  !insertmacro IsShortcutTarget "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\$OldMainBinaryName"
  Pop $0
  ${If} $0 = 1
    !insertmacro SetShortcutTarget "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    Return
  ${EndIf}

  ; Skip creating shortcut if in update mode or no shortcut mode
  ; but always create if migrating from wix
  ${If} $WixMode = 0
    ${If} $UpdateMode = 1
    ${OrIf} $NoShortcutMode = 1
      Return
    ${EndIf}
  ${EndIf}

  CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  !insertmacro SetLnkAppUserModelId "$DESKTOP\${PRODUCTNAME}.lnk"
FunctionEnd

; -----------------------------------------------------------------------------
; Strata: system checks and existing-install detection
; -----------------------------------------------------------------------------

; Shows a message in interactive mode, then quits with an error code. Silent
; and passive runs (the updater, scripted installs) only get the exit code.
!macro STRATA_FAIL MESSAGE CODE
  ${IfNot} ${Silent}
  ${AndIf} $PassiveMode <> 1
    MessageBox MB_ICONSTOP|MB_OK "${MESSAGE}"
  ${EndIf}
  SetErrorLevel ${CODE}
  Quit
!macroend

Function StrataCheckSystem
  ; NOTE: a second Setup would race the first over the same files and
  ; registry keys. The mutex lives as long as this process.
  System::Call 'kernel32::CreateMutexW(p 0, i 0, w "Local\StrataSetup") p .r0 ?e'
  Pop $1
  ${If} $1 = ${ERROR_ALREADY_EXISTS}
    !insertmacro STRATA_FAIL "Strata Setup is already running." 1
  ${EndIf}

  ${IfNot} ${AtLeastWin10}
    !insertmacro STRATA_FAIL "Strata needs Windows 10 or Windows 11." 1
  ${EndIf}

  !if "${ARCH}" == "arm64"
    ${IfNot} ${IsNativeARM64}
      !insertmacro STRATA_FAIL "This installer is for PCs with an ARM64 processor. Download the x64 installer from ${HOMEPAGE}/releases/latest instead." 1
    ${EndIf}
  !else if "${ARCH}" == "x64"
    ${IfNot} ${RunningX64}
      !insertmacro STRATA_FAIL "Strata needs a 64-bit version of Windows." 1
    ${EndIf}
    ; COMPAT: Windows 10 on ARM can't run x64 apps; Windows 11 on ARM can.
    ${If} ${IsNativeARM64}
    ${AndIfNot} ${AtLeastBuild} 22000
      !insertmacro STRATA_FAIL "This PC has an ARM64 processor. Download the ARM64 installer from ${HOMEPAGE}/releases/latest instead." 1
    ${EndIf}
  !endif
FunctionEnd

Function StrataDetectExisting
  StrCpy $ExistingState "none"
  StrCpy $ExistingVersion ""
  StrCpy $DesktopShortcut 1

  ReadRegStr $0 SHCTX "${UNINSTKEY}" ""
  ReadRegStr $1 SHCTX "${UNINSTKEY}" "UninstallString"
  ${If} "$0$1" == ""
    Return
  ${EndIf}

  ReadRegStr $ExistingVersion SHCTX "${UNINSTKEY}" "DisplayVersion"
  ${If} $ExistingVersion == ""
    ; An install without a version is treated as older, so it gets updated.
    StrCpy $ExistingState "older"
  ${Else}
    nsis_tauri_utils::SemverCompare "${VERSION}" $ExistingVersion
    Pop $0
    ${If} $0 = 0
      StrCpy $ExistingState "same"
    ${ElseIf} $0 = 1
      StrCpy $ExistingState "older"
    ${Else}
      StrCpy $ExistingState "newer"
    ${EndIf}
  ${EndIf}

  ; Keep the user's earlier choice: offer the desktop shortcut only if one exists.
  !insertmacro IsShortcutTarget "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  Pop $DesktopShortcut
FunctionEnd

; -----------------------------------------------------------------------------
; Strata: page styling
; -----------------------------------------------------------------------------

Var StrataStyleReady
Var StrataFontTitle
Var StrataFontStrong
Var StrataFontSmall
Var StrataFontBrand
Var StrataIcon

Function StrataInitStyle
  ${If} $StrataStyleReady == 1
    Return
  ${EndIf}
  CreateFont $StrataFontTitle "Segoe UI Semibold" 16 600
  CreateFont $StrataFontStrong "Segoe UI Semibold" 9 600
  CreateFont $StrataFontSmall "Segoe UI" 8 400
  CreateFont $StrataFontBrand "Segoe UI Semibold" 13 600

  ; NOTE: in high contrast mode every colour comes from the system theme, so
  ; the pages skip their own colours entirely. HIGHCONTRASTW is 12 bytes in
  ; this 32-bit process; bit 0 of dwFlags is HCF_HIGHCONTRASTON.
  StrCpy $HighContrast 0
  System::Call '*(i 12, i 0, p 0) p .r0'
  System::Call 'user32::SystemParametersInfoW(i 0x42, i 12, p r0, i 0) i .r1'
  ${If} $1 <> 0
    System::Call '*$0(i, i .r2)'
    IntOp $HighContrast $2 & 1
  ${EndIf}
  System::Free $0
  StrCpy $StrataStyleReady 1
FunctionEnd

!macro STRATA_COLORS HWND FG BG
  ${If} $HighContrast = 0
    SetCtlColors ${HWND} "${FG}" "${BG}"
  ${EndIf}
!macroend

; Creates a label with a font and colours; leaves its handle in $0.
!macro STRATA_LABEL X Y W H TEXT FONT FG BG
  ${NSD_CreateLabel} ${X} ${Y} ${W} ${H} "${TEXT}"
  Pop $0
  SendMessage $0 ${WM_SETFONT} ${FONT} 0
  !insertmacro STRATA_COLORS $0 "${FG}" "${BG}"
!macroend

; The dark brand panel on the left of the install and finish pages, built
; from native controls so it stays sharp at every display scale.
!macro STRATA_PANEL_TILE X Y W H
  ${NSD_CreateLabel} ${X} ${Y} ${W} ${H} ""
  Pop $1
  !insertmacro STRATA_COLORS $1 "" "${STRATA_PANEL}"
!macroend

Function StrataSidebar
  ; NOTE: sibling controls that overlap don't paint in a reliable order, so the
  ; panel is tiled around the icon instead of lying underneath it. The labels
  ; further down sit on the lower tile and are created after it.
  !insertmacro STRATA_PANEL_TILE 0u 0u 109u 16u
  !insertmacro STRATA_PANEL_TILE 0u 16u 14u 24u
  !insertmacro STRATA_PANEL_TILE 38u 16u 71u 24u
  !insertmacro STRATA_PANEL_TILE 0u 40u 109u 100%

  ${NSD_CreateIcon} 14u 16u 24u 24u ""
  Pop $1
  ; SS_CENTERIMAGE keeps the control at 24u x 24u and fills around the icon
  ; with the panel colour instead of shrinking to the icon's size.
  ${NSD_AddStyle} $1 0x00000200
  System::Call 'user32::GetDpiForWindow(p $HWNDPARENT) i .r2'
  ${If} $2 < 96
    StrCpy $2 96
  ${EndIf}
  IntOp $3 40 * $2
  IntOp $3 $3 / 96
  ; Icon 103 is the installer's own icon resource, i.e. the Strata icon.
  System::Call 'kernel32::GetModuleHandleW(p 0) p .r4'
  System::Call 'user32::LoadImageW(p r4, p 103, i 1, i r3, i r3, i 0) p .r5'
  StrCpy $StrataIcon $5
  SendMessage $1 ${STM_SETICON} $5 0
  !insertmacro STRATA_COLORS $1 "" "${STRATA_PANEL}"

  !insertmacro STRATA_LABEL 14u 50u 88u 16u "${PRODUCTNAME}" $StrataFontBrand "${STRATA_PANEL_TEXT}" "${STRATA_PANEL}"
  !insertmacro STRATA_LABEL 14u 68u 88u 30u "See everything on your drives." $StrataFontSmall "${STRATA_PANEL_MUTED}" "${STRATA_PANEL}"
  !insertmacro STRATA_LABEL 14u 174u 88u 10u "Open source · MIT" $StrataFontSmall "${STRATA_PANEL_MUTED}" "${STRATA_PANEL}"
FunctionEnd

Function StrataFreeIcon
  ${If} $StrataIcon != ""
  ${AndIf} $StrataIcon <> 0
    System::Call 'user32::DestroyIcon(p $StrataIcon)'
    StrCpy $StrataIcon ""
  ${EndIf}
FunctionEnd

; Sets the Next button's label and hides Back, which has nowhere to go.
!macro STRATA_BUTTONS NEXT_TEXT
  GetDlgItem $0 $HWNDPARENT 1
  SendMessage $0 ${WM_SETTEXT} 0 "STR:${NEXT_TEXT}"
  GetDlgItem $0 $HWNDPARENT 3
  ShowWindow $0 ${SW_HIDE}
!macroend

; -----------------------------------------------------------------------------
; Strata: install page
; -----------------------------------------------------------------------------

!if "${ARCH}" == "arm64"
  !define STRATA_ARCH_LABEL "ARM64"
!else
  !define STRATA_ARCH_LABEL "x64"
!endif

Var StrataPathLabel
Var StrataShortcutBox

Function StrataInstallPage
  ${If} $PassiveMode = 1
    Abort
  ${EndIf}
  Call StrataInitStyle

  ${If} $ExistingState == "older"
    StrCpy $R1 "Update Strata"
    ${If} $ExistingVersion == ""
      StrCpy $R2 "An earlier version of Strata is installed. This updates it to ${VERSION} and keeps your settings, history and rules."
    ${Else}
      StrCpy $R2 "Strata $ExistingVersion is installed. This updates it to ${VERSION} and keeps your settings, history and rules."
    ${EndIf}
    StrCpy $R3 "Update"
  ${ElseIf} $ExistingState == "same"
    StrCpy $R1 "Reinstall Strata"
    StrCpy $R2 "Strata ${VERSION} is already installed. Reinstalling restores its program files and keeps your settings, history and rules."
    StrCpy $R3 "Reinstall"
  ${ElseIf} $ExistingState == "newer"
    StrCpy $R1 "Install an older version"
    !if "${ALLOWDOWNGRADES}" == "false"
      StrCpy $R2 "Strata $ExistingVersion is installed, which is newer than this installer. Download the latest version instead."
    !else
      StrCpy $R2 "Strata $ExistingVersion is installed. This replaces it with ${VERSION}. Your settings and history are kept; options added after ${VERSION} are ignored."
    !endif
    StrCpy $R3 "Install ${VERSION}"
  ${Else}
    StrCpy $R1 "Install Strata"
    StrCpy $R2 "See what is using your disk, which app put it there and whether it’s safe to delete. Strata runs without administrator rights and asks only when a fast scan needs them."
    StrCpy $R3 "Install"
  ${EndIf}

  nsDialogs::Create 1044
  Pop $R0
  ${If} $R0 == error
    Abort
  ${EndIf}
  nsDialogs::SetRTL $(^RTL)
  !insertmacro STRATA_COLORS $R0 "" "${STRATA_BG}"

  !insertmacro STRATA_LABEL 124u 14u 190u 20u $R1 $StrataFontTitle "${STRATA_TEXT}" "${STRATA_BG}"
  !insertmacro STRATA_LABEL 124u 36u 190u 10u "Version ${VERSION} for ${STRATA_ARCH_LABEL} PCs" $StrataFontSmall "${STRATA_MUTED}" "${STRATA_BG}"
  ${NSD_CreateLabel} 124u 54u 190u 40u $R2
  Pop $0
  !insertmacro STRATA_COLORS $0 "${STRATA_TEXT}" "${STRATA_BG}"

  !insertmacro STRATA_LABEL 124u 102u 190u 10u "Install location" $StrataFontStrong "${STRATA_TEXT}" "${STRATA_BG}"
  ${NSD_CreateLabel} 124u 114u 190u 10u $INSTDIR
  Pop $StrataPathLabel
  ${NSD_AddStyle} $StrataPathLabel ${SS_PATHELLIPSIS}
  !insertmacro STRATA_COLORS $StrataPathLabel "${STRATA_MUTED}" "${STRATA_BG}"
  ${If} $ExistingState == "none"
    ${NSD_CreateLink} 124u 126u 90u 10u "Change location"
    Pop $0
    !insertmacro STRATA_COLORS $0 "${STRATA_LINK}" "${STRATA_BG}"
    ${NSD_OnClick} $0 StrataChooseFolder
  ${Else}
    ; NOTE: moving an existing install would leave the old copy behind.
    !insertmacro STRATA_LABEL 124u 126u 190u 10u "Strata stays where it is installed." $StrataFontSmall "${STRATA_MUTED}" "${STRATA_BG}"
  ${EndIf}

  ${If} $NoShortcutMode <> 1
    ${NSD_CreateCheckbox} 124u 146u 190u 12u "Create a desktop shortcut"
    Pop $StrataShortcutBox
    !insertmacro STRATA_COLORS $StrataShortcutBox "${STRATA_TEXT}" "${STRATA_BG}"
    ${If} $DesktopShortcut = 1
      ${NSD_Check} $StrataShortcutBox
    ${EndIf}
  ${EndIf}

  !insertmacro STRATA_LABEL 124u 172u 190u 16u "Free and open source under the MIT License. No telemetry." $StrataFontSmall "${STRATA_MUTED}" "${STRATA_BG}"

  Call StrataSidebar
  Call muiPageLoadFullWindow
  !insertmacro STRATA_BUTTONS $R3
  !if "${ALLOWDOWNGRADES}" == "false"
    ${If} $ExistingState == "newer"
      GetDlgItem $0 $HWNDPARENT 1
      EnableWindow $0 0
    ${EndIf}
  !endif
  nsDialogs::Show
  Call muiPageUnloadFullWindow
  Call StrataFreeIcon
FunctionEnd

Function StrataInstallPageLeave
  ${If} $NoShortcutMode <> 1
    ${NSD_GetState} $StrataShortcutBox $0
    ${If} $0 = ${BST_CHECKED}
      StrCpy $DesktopShortcut 1
    ${Else}
      StrCpy $DesktopShortcut 0
    ${EndIf}
  ${EndIf}

  ; Free space on the target drive, in MB, against the installed size plus a
  ; margin for the WebView2 bootstrapper and logs.
  StrCpy $1 $INSTDIR 3
  ClearErrors
  ${DriveSpace} $1 "/D=F /S=M" $2
  ${IfNot} ${Errors}
    IntOp $3 ${ESTIMATEDSIZE} / 1024
    IntOp $3 $3 + 64
    ${If} $2 < $3
      MessageBox MB_ICONEXCLAMATION|MB_OK "There isn’t enough free space on $1. Strata needs about $3 MB; $2 MB is free."
      Abort
    ${EndIf}
  ${EndIf}
FunctionEnd

Function StrataChooseFolder
  Pop $0
  nsDialogs::SelectFolderDialog "Choose where to install Strata. A Strata folder is created inside the folder you pick." $INSTDIR
  Pop $0
  ${If} $0 == error
  ${OrIf} $0 == ""
    Return
  ${EndIf}

  StrCpy $1 $0 1 -1
  ${If} $1 == "\"
    StrCpy $0 $0 -1
  ${EndIf}

  ; Local drives only: "X:" followed by an optional path.
  StrCpy $1 $0 1 1
  ${If} $1 != ":"
    MessageBox MB_ICONEXCLAMATION|MB_OK "Choose a folder on a drive in this PC. Strata can’t be installed on a network location."
    Return
  ${EndIf}

  ${GetFileName} $0 $1
  ${If} $1 != "${PRODUCTNAME}"
    StrCpy $0 "$0\${PRODUCTNAME}"
  ${EndIf}

  StrLen $2 $WINDIR
  StrCpy $1 $0 $2
  StrCpy $3 $0 1 $2
  ${If} $1 == $WINDIR
  ${AndIf} $3 == "\"
    MessageBox MB_ICONEXCLAMATION|MB_OK "Strata can’t be installed inside the Windows folder. Choose another folder."
    Return
  ${EndIf}

  StrLen $1 $0
  ${If} $1 > 180
    MessageBox MB_ICONEXCLAMATION|MB_OK "That path is too long. Choose a folder with a shorter path."
    Return
  ${EndIf}

  StrCpy $INSTDIR $0
  ${NSD_SetText} $StrataPathLabel $INSTDIR
FunctionEnd

; -----------------------------------------------------------------------------
; Strata: progress and finish pages
; -----------------------------------------------------------------------------

Function StrataProgressShow
  ${If} $ExistingState == "older"
    !insertmacro MUI_HEADER_TEXT "Updating Strata" "Updating to version ${VERSION}. This takes a few seconds."
  ${ElseIf} $ExistingState == "same"
    !insertmacro MUI_HEADER_TEXT "Reinstalling Strata" "This takes a few seconds."
  ${ElseIf} $ExistingState == "newer"
    !insertmacro MUI_HEADER_TEXT "Installing Strata ${VERSION}" "This takes a few seconds."
  ${Else}
    !insertmacro MUI_HEADER_TEXT "Installing Strata" "This takes a few seconds."
  ${EndIf}
FunctionEnd

Var StrataLaunchBox

Function StrataFinishPage
  ${If} $PassiveMode = 1
    Abort
  ${EndIf}
  Call StrataInitStyle

  ${If} $ExistingState == "older"
    StrCpy $R1 "Strata is updated"
    StrCpy $R2 "You’re on version ${VERSION}. Your settings, history and rules were kept."
  ${ElseIf} $ExistingState == "same"
    StrCpy $R1 "Strata is reinstalled"
    StrCpy $R2 "Strata ${VERSION} is ready to use."
  ${ElseIf} $ExistingState == "newer"
    StrCpy $R1 "Strata ${VERSION} is installed"
    StrCpy $R2 "Strata checks for updates on its own, so it will offer the latest version again."
  ${Else}
    StrCpy $R1 "Strata is installed"
    StrCpy $R2 "Find it in the Start menu. When a fast scan needs administrator rights, Windows asks you first."
  ${EndIf}

  nsDialogs::Create 1044
  Pop $R0
  ${If} $R0 == error
    Abort
  ${EndIf}
  nsDialogs::SetRTL $(^RTL)
  !insertmacro STRATA_COLORS $R0 "" "${STRATA_BG}"

  !insertmacro STRATA_LABEL 124u 14u 190u 20u $R1 $StrataFontTitle "${STRATA_TEXT}" "${STRATA_BG}"
  ${NSD_CreateLabel} 124u 40u 190u 30u $R2
  Pop $0
  !insertmacro STRATA_COLORS $0 "${STRATA_TEXT}" "${STRATA_BG}"

  ${NSD_CreateCheckbox} 124u 80u 190u 12u "Launch Strata"
  Pop $StrataLaunchBox
  !insertmacro STRATA_COLORS $StrataLaunchBox "${STRATA_TEXT}" "${STRATA_BG}"
  ${NSD_Check} $StrataLaunchBox

  ${NSD_CreateLink} 124u 172u 120u 10u "What’s new in ${VERSION}"
  Pop $0
  !insertmacro STRATA_COLORS $0 "${STRATA_LINK}" "${STRATA_BG}"
  ${NSD_OnClick} $0 StrataOpenReleaseNotes

  Call StrataSidebar
  Call muiPageLoadFullWindow
  !insertmacro STRATA_BUTTONS "Finish"
  ; NOTE: as on Modern UI's own finish page, a disabled Cancel also disables
  ; the close button, so closing can't raise the "quit Setup" warning after a
  ; successful install.
  GetDlgItem $0 $HWNDPARENT 2
  EnableWindow $0 0
  nsDialogs::Show
  Call muiPageUnloadFullWindow
  Call StrataFreeIcon
FunctionEnd

Function StrataFinishPageLeave
  ${NSD_GetState} $StrataLaunchBox $0
  ${If} $0 = ${BST_CHECKED}
    Call RunMainBinary
  ${EndIf}
FunctionEnd

Function StrataOpenReleaseNotes
  Pop $0
  ; SECURITY: Setup runs elevated; the browser must open as the signed-in user.
  nsis_tauri_utils::RunAsUser "$WINDIR\explorer.exe" "${STRATA_RELEASE_URL}"
FunctionEnd
