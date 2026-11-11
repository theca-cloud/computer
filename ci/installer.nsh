; Tauri's NSIS hooks (tauri.conf.json, bundle.windows.nsis.installerHooks).
; The uninstaller removes every drive first, as `theca remove` would: no
; sync root stays registered (a leftover one turns a later folder at that
; path into a broken cloud folder), edits not uploaded yet moved out.
; Not when an installer replaces this version ("Uninstall before
; installing", the upgrade's first choice): it runs this uninstaller in
; place (`_?=`), from the install folder, where a user's uninstall runs a
; copy of it from the temp folder.
!macro NSIS_HOOK_PREUNINSTALL
  ${If} $EXEDIR != $INSTDIR
    ExecWait '"$INSTDIR\theca.exe" remove-drives'
  ${EndIf}
!macroend
