# Internet Download Manager (IDM) — static analysis notes

Source: IDM v6.43 build 18 installed at `C:\Program Files (x86)\Internet Download Manager` on the author's Windows PC,
examined with REA (PE imports, resources, embedded strings, class names), the registry, and the on-disk files.
Runtime behavior is in `idm-behavior-spec.md`. **No IDM binaries, code, strings tables or data files are in this repo and
none must be added** — ZDM is a clean-room implementation of the *observable behavior*. IDM is Windows-only; nothing here
can be re-checked on Linux. If a question needs the binaries, ask the owner to look on the Windows machine.

Confidence: component roles are **inferred** from file names, imports, class names and strings. Nothing was decompiled
and IDM's download engine code was not read.

## Components (install folder)

| Layer | Files | Role (inferred) |
|---|---|---|
| Main app | `IDMan.exe` (6.5 MB, 32-bit MFC GUI) | Download engine, queue/scheduler, "Grabber" site crawler (`CIDMSpider`), UI, own HTTP/HTTPS stack |
| Browser integration | `IDMMsgHost.exe` (+ `IDMMsgHost.json`, `IDMMsgHostMoz.json`), `IDMGCExt.crx`, `IDMEdgeExt.crx`, `IDMOpExt.nex`, `idmmzcc*.xpi`, `idmcchandler*`, `idmmzcc*.dll`, `IDMIECC*`, `idmbrbtn*`, `IDMGetAll*`, `downlWithIDM*` | Catch downloads/media in Chrome, Edge, Opera, Firefox, IE; "Download with IDM" buttons |
| Kernel interception | `idmwfp64.sys`/`idmwfp32.sys` (Windows Filtering Platform callout), `idmtdi64.sys`/`idmtdi32.sys` (older TDI), `IDMNetMon*.dll` | Optional "Advanced Browser Integration": capture HTTP requests system-wide |
| Helpers | `IDMGrHlp.exe`, `IDMIntegrator64.exe`, `MediumILStart.exe`, `idmBroker.exe`, `IEMonitor.exe` | Grabber helper, integration setup, medium-integrity launcher |
| Other DLLs | `IDMShellExt*` (Explorer menu), `idmvconv` (video convert), `IDMVMPrs*` (media page parsers), `idmftype*`, `idmindex`, `libssl`, `libcrypto` | Shell integration, media, file types, TLS |
| Data | `defexclist.txt` (URL-pattern exclusions), `idmfc.dat`, `IDMFType.dat`, `Languages\*.lng`, `*.chm` help | |

## Native messaging
`IDMMsgHost.json` registers host name `com.tonec.idm`, stdio, allowed to two specific Chrome extension IDs; the Firefox
manifest allows one extension ID. `IDMMsgHost.exe` strings: `\\.\pipe\IDMNetworkMonitor.<n>`, `idmcmd://startnetmon/`,
`Local\IDMEventMonitor`. Inferred: the host relays to `IDMan.exe`/`IDMNetMon` over the pipe.
(ZDM's equivalent: `crates/zdm-nmhost`, host name `com.zdm.host`.)

## Kernel drivers
- `idmwfp*.sys` imports `FwpmCalloutAdd0`, `FwpsCalloutRegister0/1`, `FwpsRedirectHandleCreate0`, `FwpsAcquireWritableLayerDataPointer0`,
  device `\Device\IDMWFP`. `idmtdi*.sys` sits on `\Device\Tcp`, device `\Device\IDMTDI`.
- Both contain `X-IDM-ProcessId: ` and `426 Intercepted by the IDM Advanced Integration` (TDI also
  `HTTP/1.0 204 Intercepted by the IDM Advanced Integration`). Inferred: tag the originating process, stop the browser's own
  transfer so IDM takes over. Registry `EnableDriver=1`.
- `IDMNetMon*.dll` has HTTP parsers (`FullHTTPRequest`, `ContentParser`, `ContentDecoder`) and opens `\\.\IDMWFP`.
- ZDM decision: **no drivers**; the browser extension covers capture. Not worth the risk on Linux.

## IDMan.exe
- Imports (KERNEL32, USER32, GDI32, COMDLG32, ADVAPI32, COMCTL32, SHLWAPI) include `CreateFileMapping`, `LockFile`,
  `SetFilePointerEx`, `SetEndOfFile`, `CreateThread` (segment files/threads), `CreateProcessAsUserW`, `DuplicateTokenEx`
  (medium-integrity launches), `OpenSCManagerA`/`OpenServiceA` + `DeviceIoControl` (driver service), `SetWindowsHooksExA`,
  `RegSetValueEx*`.
- MSVC RTTI class names: `HTTPConnection`, `HTTPSession`, `CHttpDateHeader`, `CDownloadQueue`, `CQueuesArray`, `CQueueTab`,
  `CSchedulerInfo`, `CGrabbersSchedulers`, `CIDMSpider*` (+ settings/site-map dialogs), `CCustomGrabberTemplates`,
  `CVirusCheckingSettingsDlg`, `CIDMProxyScript`, `CIDMProxySettings2`, `CIDMCacheDatabase`, `CSharedFile`,
  `CIDMLinkTransmitter` (COM, browser -> app link handoff).
- Strings: `%s %s HTTP/1.1`, `#EXT-X-BYTERANGE:` (HLS), custom TLS handshake logging (`AcquireCredentialsHandle`),
  `\\.\IDMTDI`, `\\.\IDMWFP`, driver paths `%s\drivers\idmtdi.sys` / `idmwfp.sys`, per-browser cookie DB readers
  (Brave, Opera), `%s\Scheduler\q_%s.dt` / `s_%s.dt`, antivirus hook text ("IDM will execute antivirus software
  appending the complete path of a downloaded file"), per-site exclusion globs (`*.rapidshare.com`, ...).
- It also embeds a large block of generic English/Spanish web text and HTML/jQuery fragments; purpose unknown (possibly a
  word list/dictionary used for content detection). Not investigated.

## Registry (`HKCU\Software\DownloadManager`) — representative values
`AppDataIDMFolder` (`%AppData%\IDM\`), `CommonAppDataIDMFolder` (`C:\ProgramData\IDM\`), `ExePath`, `idmvers`,
`EnableDriver`, `Extensions` (about 60 intercepted file types: 3GP 7Z AAC ... ZIP DOCX PPTX XLSX MSIX APK CAB ...),
`ExceptionServers` (Windows Update etc.), `PanelExceptionServers`, `UseHttpProxy`, `UseFtpProxy`, `FtpPasive`,
`MonitorUrlClipboard`, `LaunchOnStart`, `ToolbarStyle`, `LargeButtons`, `TrayIcon`.

## On-disk layout observed at runtime
`%AppData%\IDM\DwnlData\<user>\<name>_<id>\` holds `<name>` (first segment), `<name>1..<name>8` (per-connection parts),
`<name>_<id>.log` (plain text: URL, "User : anonymous", timestamps, per-connection raw response headers).
`%AppData%\IDM\Scheduler\q_*.dt` queue state. `defextmap.dat`, `urlexclist.dat` alongside.

## Features IDM has that ZDM has / has not copied
Has in ZDM: segmented resumable download, queues with time windows, categories, pause/resume, browser takeover, cookies/referer/UA
forwarding, proxy, Basic auth, rate limit, crash recovery.
Not yet: site grabber/spider, HLS/video capture, antivirus hook, proxy auto-config script, FTP, "download all links",
shell integration, per-queue completion actions, clipboard monitoring.
