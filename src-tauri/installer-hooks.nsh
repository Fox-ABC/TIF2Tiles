!macro NSIS_HOOK_POSTINSTALL
  ; 安装后注册 xuntian-uploader 协议，确保浏览器可把 URL 交给当前安装的 exe。
  FindFirst $0 $1 "$INSTDIR\*.exe"
  StrCmp $1 "" done
  WriteRegStr SHCTX "Software\Classes\xuntian-uploader" "" "URL:xuntian-uploader Protocol"
  WriteRegStr SHCTX "Software\Classes\xuntian-uploader" "URL Protocol" ""
  WriteRegStr SHCTX "Software\Classes\xuntian-uploader\DefaultIcon" "" "$INSTDIR\$1,0"
  WriteRegStr SHCTX "Software\Classes\xuntian-uploader\shell" "" ""
  WriteRegStr SHCTX "Software\Classes\xuntian-uploader\shell\open" "" ""
  WriteRegStr SHCTX "Software\Classes\xuntian-uploader\shell\open\command" "" '"$INSTDIR\$1" "%1"'
done:
  FindClose $0
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; 卸载时移除协议注册，避免系统残留无效打开方式。
  DeleteRegKey SHCTX "Software\Classes\xuntian-uploader"
!macroend
