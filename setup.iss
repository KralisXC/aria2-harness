; ============================================================================
;  Aria2-Harness 安装包脚本（Inno Setup 7）
;  编译（双版本）：
;    Full（内置 WebView2 运行时，约 230MB）：ISCC.exe setup.iss
;    Lite（不含 WebView2Runtime，约 15MB）： ISCC.exe /DLITE setup.iss
;  产物：installer_output\Aria2-Harness-Setup-1.0.0-rc1-x64-Full.exe
;        installer_output\Aria2-Harness-Setup-1.0.0-rc1-x64-Lite.exe
;
;  beta8 ~ beta8.2 覆盖安装数据安全（Full 与 Lite 任意互切）：
;    覆盖安装只替换程序本体（exe / dll / dist / browser-extension / WebView2Runtime），
;    绝不触碰 data\（任务列表 / session / SQLite）、plugins\、downloads\、config.json 与
;    %LOCALAPPDATA%\Aria2-Harness（安装版的用户数据根）。
;    [Dirs] 全部带 uninsneveruninstall；[InstallDelete] 只清程序资源目录。
;
;  说明：源文件直接取绿色便携包目录，保证"便携包 / 安装版"内容同源，
;        不会出现两套产物各自漂移。
;        Lite 版只剔除 WebView2Runtime；VC++ 运行库（msvcp140 等）仍随包，
;        宿主启动时若找不到 WebView2Runtime 自动改用系统 WebView2。
; ============================================================================

#define MyAppName        "Aria2-Harness"
#define MyAppVersion     "1.0.0-rc1"
#define MyAppPublisher   "lingxi"
#define MyAppExeName     "aria2-harness.exe"
; 版本分支：/DLITE 编译轻量版，其余一律完整版。
; beta12.4：便携包按 Full / Lite 分目录组装，源目录随之切换。
; 1.0.0-rc1：源目录切到 build\rc1（结构不变）。
#ifdef LITE
  #define MyEdition "Lite"
  #define MyPkgDir  "build\rc1\Aria2-Harness-v1.0.0-rc1-Windows-x64\Lite"
#else
  #define MyEdition "Full"
  #define MyPkgDir  "build\rc1\Aria2-Harness-v1.0.0-rc1-Windows-x64\Full"
#endif
; 固定 AppId：升级安装靠它识别同一产品，**发布后不可再改**。
; 装 v1.0-beta6 时 Inno 看到同一个 AppId 会走「覆盖安装」，不会出现"两个 Aria2-Harness"。
#define MyAppId          "{{B7C4F2A1-3D6E-4F8B-9C2A-5E7D1A0B3C48}"

[Setup]
AppId={#MyAppId}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
; 写入 Program Files 需要管理员；服务进程本身也要求管理员（falloc 依赖特权）。
PrivilegesRequired=admin
; Inno Setup 7 推荐 x64compatible（同时覆盖 x64 与 ARM64 上的 x64 模拟）；
; 旧写法 x64 在 Inno 7 已标记为 deprecated（会被自动替换成 x64os）。
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=installer_output
OutputBaseFilename=Aria2-Harness-Setup-{#MyAppVersion}-x64-{#MyEdition}
SetupIconFile=client\src-host\assets\app.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName} {#MyAppVersion} ({#MyEdition})
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; 覆盖升级时若旧版仍在运行，交由 Restart Manager 优雅关闭（避免"文件被占用"）。
; 但 Restart Manager 关不掉托盘里的进程（关闭 = 收进托盘，进程仍在），
; 所以真正的兜底是 [Code] 里的 PrepareToInstall：taskkill /F 三个可执行文件。
CloseApplications=yes
RestartApplications=no
; 升级时沿用上次的安装目录与任务勾选，避免用户改过路径后被重置。
UsePreviousAppDir=yes
UsePreviousTasks=yes
; 安装包本身也要求 Win10+（WebView2 / UCRT 的既有前提）。
MinVersion=10.0
; 语言自适应：按系统区域自动挑选 .isl，绝不弹语言选择框。
; 中文系统 → ChineseSimplified.isl；其他 → 英文 Default.isl。
ShowLanguageDialog=no

[Languages]
; 顺序即"系统区域匹配失败"时的回退顺序：先英文，符合国际惯例。
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[CustomMessages]
; 卸载时的自有文案（内置向导文案由 .isl 提供，这里只放脚本自定义字符串）。
english.DeleteUserDataPrompt=Also delete your download records, settings and plugin data?%n%nYes: remove %1%nNo: keep them for a future reinstall
chinesesimplified.DeleteUserDataPrompt=是否同时删除您的下载记录、配置和插件数据？%n%n「是」：彻底删除 %1%n「否」：保留数据，便于日后重装
english.DeleteUserDataFailed=Could not delete: %1%nClose Aria2-Harness and delete the folder manually.
chinesesimplified.DeleteUserDataFailed=无法删除：%1%n请先关闭 Aria2-Harness 后手动删除该目录。

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
; —— 三件套 ——
Source: "{#MyPkgDir}\aria2-harness.exe";  DestDir: "{app}"; Flags: ignoreversion
Source: "{#MyPkgDir}\aria2-service.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#MyPkgDir}\aria2c.exe";        DestDir: "{app}"; Flags: ignoreversion
; —— MSVC 运行库（应用本地部署，免装 VC++ Redist）——
; aria2c.exe 是 MinGW 静态链接产物，导入表只有系统 DLL，无需额外库；
; 下面三件是 Rust 宿主 / WebView2 加载器的兜底：精简版系统常把 VC++ Redist 裁掉，
; 缺一个 vcruntime140 就是"双击无反应"（进程加载阶段即失败，连 panic 都没机会执行）。
Source: "{#MyPkgDir}\VCRUNTIME140.dll";   DestDir: "{app}"; Flags: ignoreversion
Source: "{#MyPkgDir}\VCRUNTIME140_1.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#MyPkgDir}\msvcp140.dll";       DestDir: "{app}"; Flags: ignoreversion
; —— WebView2 加载器 ——
; 必须与 aria2-harness.exe 同目录：DLL 搜索顺序第一站是 exe 所在目录。
Source: "{#MyPkgDir}\WebView2Loader.dll"; DestDir: "{app}"; Flags: ignoreversion
; —— 前端静态资源（宿主内置 127.0.0.1 静态服务器托管，必须随包）——
Source: "{#MyPkgDir}\dist\*"; DestDir: "{app}\dist"; Flags: ignoreversion recursesubdirs createallsubdirs
; —— 浏览器扩展（设置 → 网络「安装浏览器扩展」引导用户加载此目录）——
Source: "{#MyPkgDir}\browser-extension\*"; DestDir: "{app}\browser-extension"; Flags: ignoreversion recursesubdirs createallsubdirs
; —— beta12.3：Full 版内置 FFmpeg 业务插件 + 共享组件包 ——
; 宿主扫描的是「数据根/data/plugins」而非安装目录：安装到 Program Files 时数据根在
; %LOCALAPPDATA%\Aria2-Harness，便携布局则跟随 {app}（见 src/paths.rs 的 detect_home）。
; Lite 版不包含任何插件与共享池：用户按 README 下载对应 .zip 解压到同一目录即可。
;
; 注意：本段 Source 是**唯一**不取 {#MyPkgDir} 的条目，按脚本所在目录（仓库根）解析。
; 插件源文件只有一份（data\plugins\），构建时无需再往便携包目录里复制，
; 避免"仓库 / 便携包"两份插件各自漂移。
;
; 插件本体只含 manifest.json / main.js（体积 < 20KB），ffmpeg.exe 与 MinGW 运行库
; 平铺在 data\plugins\.shared\com.harness.shared-ffmpeg（与 manifest.json 同级，beta12.3 去 bin），
; 宿主 exec::resolve_program 按「插件 bin → .shared/<共享id>/ → .shared/<共享id>/bin → 系统 PATH」顺序查找。
; 点目录不会被插件扫描器当成插件（manager 跳过 `.` 开头的目录）。
#ifndef LITE
Source: "data\plugins\.shared\com.harness.shared-ffmpeg\*"; DestDir: "{code:PluginDataDir}\.shared\com.harness.shared-ffmpeg"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "data\plugins\hls-downloader\*"; DestDir: "{code:PluginDataDir}\hls-downloader"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "data\plugins\m4s-merger\*";    DestDir: "{code:PluginDataDir}\m4s-merger";    Flags: ignoreversion recursesubdirs createallsubdirs
Source: "data\plugins\live-recorder\*"; DestDir: "{code:PluginDataDir}\live-recorder"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "data\plugins\media-converter\*"; DestDir: "{code:PluginDataDir}\media-converter"; Flags: ignoreversion recursesubdirs createallsubdirs
#endif
; —— WebView2 Fixed Version 运行时（仅 Full 版内置）——
; 精简版 Win11 常把 Evergreen 运行时裁掉，导致 wry 建不出控制器、双击毫无反应。
; 随包内置固定版本后，宿主通过 WEBVIEW2_BROWSER_EXECUTABLE_FOLDER 指向本目录，
; 完全离线、不依赖 Edge 更新器。整个目录必须原样递归安装（含 locales / 所有 dll）。
; Lite 版（/DLITE）不打包本目录：宿主检测不到 WebView2Runtime 时自动改用系统 WebView2。
#ifndef LITE
Source: "{#MyPkgDir}\WebView2Runtime\*"; DestDir: "{app}\WebView2Runtime"; Flags: ignoreversion recursesubdirs createallsubdirs
#endif
; —— 说明文档 ——
Source: "{#MyPkgDir}\使用说明.txt"; DestDir: "{app}"; Flags: ignoreversion
; —— 用户数据绝不随包覆盖 ——
; config.json 位于 %LOCALAPPDATA%\Aria2-Harness（便携版为 exe 同级），由宿主首次
; 启动时生成、设置保存时合并写入。本段只装程序本体：任何情况下都不要为它添加
; Source 条目，更不能用空模板覆盖（覆盖安装会因此把用户设置重置）。

[Registry]
; 记录本次安装的版本分支（Full / Lite）。用途只有一个：InitializeSetup 区分
; 「同版本同分支的重复安装」（拒绝）与「同版本换分支的覆盖安装」（必须放行）。
Root: HKLM64; Subkey: "Software\Aria2-Harness"; ValueType: string; ValueName: "Edition"; ValueData: "{#MyEdition}"; Flags: uninsdeletevalue

[InstallDelete]
; 覆盖安装前只清理「程序本体」中可能残留旧文件的目录（beta8 数据安全红线）：
;   - dist / browser-extension：每次都由 [Files] 重新铺开，先删避免旧 chunk 残留；
;   - Lite 额外清理 WebView2Runtime：从 Full 换到 Lite 时不该继续占几百 MB。
; **数据目录绝不在此段落出现**：data\（任务 / session / SQLite）、plugins\、
; downloads\、logs\、config.json 与 %LOCALAPPDATA%\Aria2-Harness 必须跨
; Full / Lite 覆盖原样保留。
Type: filesandordirs; Name: "{app}\dist"
Type: filesandordirs; Name: "{app}\browser-extension"
#ifdef LITE
Type: filesandordirs; Name: "{app}\WebView2Runtime"
#endif

[Dirs]
; 安装目录下的 data 只是占位与保护：Program Files 对普通用户只读，实际可写目录
; 由宿主自动落到 %LOCALAPPDATA%\Aria2-Harness（见 src/paths.rs）。
; **uninsneveruninstall 是 beta8 的数据红线**：即使 [UninstallDelete] 或未来某次
; 改动误删，这些目录也不会在卸载 / 覆盖安装时被移除。
;
; beta10.1：绝不在此创建 plugins / downloads —— 插件与下载数据的真实位置统一是
; 「数据根/data/plugins」「数据根/data/downloads」。旧版曾在此留下两个空目录，
; 用户看到安装目录里凭空多出两个空文件夹，纯属误导。
Name: "{app}\data";      Flags: uninsneveruninstall
; 安装版的用户数据根（config.json / data / WebView2 存储所在）：
; 同样禁止卸载器自动删除，只有用户在卸载弹窗里明确选「是」才由 [Code] 清理。
Name: "{localappdata}\Aria2-Harness"; Flags: uninsneveruninstall

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\卸载 {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
; —— 首选（增量 53）：注册为 Windows 系统服务（SCM）——
; 服务以 LocalSystem 身份运行，UI 永远普通权限，通过命名管道下指令。
; 升级时先 stop（未运行会返回 1062，Inno 忽略非 0 退出码，不中断安装）；
; config / create 两条互补：已存在则更新 binPath / 启动类型，不存在则新建。
Filename: "{sys}\sc.exe"; Parameters: "stop Aria2-Harness-Service"; Flags: runhidden waituntilterminated
Filename: "{sys}\sc.exe"; Parameters: "config Aria2-Harness-Service binPath= ""{app}\aria2-service.exe"" start= auto DisplayName= ""Aria2-Harness Service"""; Flags: runhidden waituntilterminated
Filename: "{sys}\sc.exe"; Parameters: "create Aria2-Harness-Service binPath= ""{app}\aria2-service.exe"" start= auto DisplayName= ""Aria2-Harness Service"""; Flags: runhidden waituntilterminated
; DACL：放行 Authenticated Users 的「查询 + 启动」权限。
; 不设这条，普通权限 UI 执行 `sc start` 会被 SCM 直接拒绝（Access Denied），
; 只能退回弹 UAC —— 那就失去了系统服务的意义。
Filename: "{sys}\sc.exe"; Parameters: "sdset Aria2-Harness-Service D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPWPDTLOCRRC;;;AU)"; Flags: runhidden waituntilterminated
; 立即启动一次，避免用户首次打开 UI 时再等一次。
Filename: "{sys}\sc.exe"; Parameters: "start Aria2-Harness-Service"; Flags: runhidden waituntilterminated
; —— 次选：注册「登录时以最高权限运行」的计划任务 ——
; 兼容 SCM 被禁用 / 服务被人为删除的环境；UI 侧按 SCM → 计划任务 → UAC 顺序回退。
; 任务由安装程序（管理员）创建，之后普通权限的 UI 只需 `schtasks /run`，
; 由任务计划服务代为提权，Windows 不会再弹 UAC。
; 卸载时在 [UninstallRun] 中删除该任务。
Filename: "{sys}\schtasks.exe"; Parameters: "/create /tn ""Aria2-Harness-Service"" /tr ""{app}\aria2-service.exe"" /sc ONLOGON /rl HIGHEST /f"; Flags: runhidden waituntilterminated
; 安装完成后立即启动一次，避免用户首次打开 UI 时再等一次提权。
Filename: "{sys}\schtasks.exe"; Parameters: "/run /tn ""Aria2-Harness-Service"""; Flags: runhidden waituntilterminated
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; 卸载时先停服并删除 SCM 注册项（此时卸载程序仍以管理员身份运行）。
; 服务不存在时 sc 返回 1060，属正常情况（静默忽略）。
Filename: "{sys}\sc.exe"; Parameters: "stop Aria2-Harness-Service"; Flags: runhidden waituntilterminated; RunOnceId: "StopWindowsService"
Filename: "{sys}\sc.exe"; Parameters: "delete Aria2-Harness-Service"; Flags: runhidden waituntilterminated; RunOnceId: "DeleteWindowsService"
; 卸载时删除计划任务（老版本升级上来的残留也要清）。
Filename: "{sys}\schtasks.exe"; Parameters: "/delete /tn ""Aria2-Harness-Service"" /f"; Flags: runhidden waituntilterminated; RunOnceId: "DeleteServiceTask"

[UninstallDelete]
; 只清理**安装目录内**的运行期残留（含空目录）。{app} 之外的路径一律不在此段出现：
;   用户数据（下载记录 / 配置 / 插件 / WebView2 存储）在 %LOCALAPPDATA%\Aria2-Harness，
;   覆盖安装与卸载都绝不触碰它；只有卸载时用户在弹窗里明确选「是」，才由 [Code] 删除。
;
; beta7：data / plugins / downloads **不再是卸载清单**。
;   安装到 Program Files 之外（如 D:\Aria2-Harness）时，宿主判定为「便携布局」，
;   真实数据就落在 {app}\data —— 无条件删除会让「卸载旧版再装新版」这条路径丢任务。
;   是否删除改由下面的弹窗决定（见 DeleteUserDataAll）。
Type: filesandordirs; Name: "{app}\logs"
; 兜底：宿主可能把崩溃日志写在安装根目录（如 aria2-service.log）。
Type: filesandordirs; Name: "{app}\*.log"

[Code]
const
  AutostartRunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  AutostartValueName = 'Aria2Harness';
  { 卸载信息键：Inno 用 AppId（含花括号）+ "_is1" 作为子键名，DisplayVersion
    即 Setup 段写入的 AppVersion。admin 安装落在 HKLM，这里 HKLM64/HKLM32/HKCU
    三处都查，避免 32 位视图或历史用户级安装漏判。 }
  UninstallKey = 'SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{B7C4F2A1-3D6E-4F8B-9C2A-5E7D1A0B3C48}_is1';

{ 安装是否落在系统目录（Program Files）：决定宿主数据根的位置。 }
function IsSystemInstall(): Boolean;
var
  AppDir, PfDir: String;
begin
  AppDir := Lowercase(ExpandConstant('{app}'));
  PfDir := Lowercase(ExpandConstant('{autopf}'));
  Result := Pos(PfDir, AppDir) = 1;
end;

{ beta10：Full 版内置插件的目标目录 —— 必须与宿主 `paths::plugins_dir()` 一致。
  Program Files 安装 → %LOCALAPPDATA%\Aria2-Harness\data\plugins；
  非系统盘（便携布局）→ 安装目录\data\plugins。 }
function PluginDataDir(Param: String): String;
begin
  if IsSystemInstall() then
    Result := ExpandConstant('{localappdata}\Aria2-Harness\data\plugins')
  else
    Result := ExpandConstant('{app}\data\plugins');
end;

function UserDataDir(): String;
begin
  Result := ExpandConstant('{localappdata}\Aria2-Harness');
end;

{ 同版本防御：已装 DisplayVersion == 本包版本（{#MyAppVersion}）时直接退出，
  避免用户重复安装做无用功。老版本（如 1.0.0）则放行走覆盖升级。 }
function InstalledVersion(): String;
var
  Version: String;
begin
  Result := '';
  if RegQueryStringValue(HKLM64, UninstallKey, 'DisplayVersion', Version) and (Version <> '') then
    Result := Version
  else if RegQueryStringValue(HKLM32, UninstallKey, 'DisplayVersion', Version) and (Version <> '') then
    Result := Version
  else if RegQueryStringValue(HKCU, UninstallKey, 'DisplayVersion', Version) and (Version <> '') then
    Result := Version;
end;

{ 已安装的版本分支（Full / Lite）。旧版本安装（无该值）返回空串。 }
function InstalledEdition(): String;
var
  Value: String;
begin
  Result := '';
  if RegQueryStringValue(HKLM64, 'Software\Aria2-Harness', 'Edition', Value) and (Value <> '') then
    Result := Value
  else if RegQueryStringValue(HKLM32, 'Software\Aria2-Harness', 'Edition', Value) and (Value <> '') then
    Result := Value
  else if RegQueryStringValue(HKCU, 'Software\Aria2-Harness', 'Edition', Value) and (Value <> '') then
    Result := Value;
end;

{ 安装前只做一件事：同版本同分支的重复安装拦截。

  beta7 起必须允许 Full 与 Lite 任意互切：两者版本号相同，但只有一个分支是
  「已经装好了」，另一个分支是用户主动换包 —— 若一律拒绝，用户就永远换不了版本。
  覆盖安装只替换程序本体，[InstallDelete] 明确不触碰 data / plugins / downloads。

  v1.1.0 起 WebView2 改为随包内置的 Fixed Version（安装目录下的 WebView2Runtime），
  不再检查 / 静默安装系统 Evergreen 运行时 —— 精简版系统也能直接双击秒开。 }
function InitializeSetup(): Boolean;
var
  Installed: String;
  InstalledEd: String;
begin
  Result := True;

  { 同版本 + 同分支才拒绝；同版本不同分支放行（Full 与 Lite 覆盖互切）。 }
  Installed := InstalledVersion();
  if Installed = '{#MyAppVersion}' then
  begin
    InstalledEd := InstalledEdition();
    if InstalledEd = '{#MyEdition}' then
    begin
      if not (WizardSilent) then
        MsgBox('您已经安装了最新版本 Aria2-Harness (v{#MyAppVersion} · {#MyEdition})，无需重复安装。'
          + #13#10 + #13#10
          + '如需在 Full 与 Lite 之间切换，请直接运行另一个版本的安装包（会自动覆盖并保留全部下载任务与设置）。',
          mbInformation, MB_OK);
      Result := False;
      Exit;
    end;
  end;
end;

{ 升级保障：结束仍在运行的三个进程，释放 exe / dll / sqlite 的文件锁。
  为什么必须 /F：主窗口的「关闭」是收进托盘（进程仍然活着），优雅关闭无法退出进程。
  taskkill 找不到进程时返回非 0 —— 属正常情况：不弹窗、不中断安装。 }
procedure KillProcess(const ImageName: String);
var
  ResultCode: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'),
    '/F /IM ' + ImageName + ' /T', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

procedure KillAppProcesses();
begin
  KillProcess('aria2-harness.exe');
  KillProcess('aria2-service.exe');
  KillProcess('aria2c.exe');
  { 等内核真正回收句柄（exe 映射 / sqlite / 日志）后再覆盖文件。 }
  Sleep(600);
end;

{ 删除「登录时以最高权限运行」的计划任务。
  卸载阶段必须在删文件前调用：任务若还在，计划任务服务会持有安装目录下
  aria2-service.exe 的引用，后续 [UninstallDelete] 会报"文件被占用"。
  任务不存在时 schtasks 返回非 0，属正常情况（静默忽略）。 }
procedure DeleteServiceTask();
var
  ResultCode: Integer;
begin
  Exec(ExpandConstant('{sys}\schtasks.exe'),
    '/delete /tn "Aria2-Harness-Service" /f', '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode);
end;

{ 删除「用户数据」——两套布局都要覆盖：
  1. %LOCALAPPDATA%\Aria2-Harness：装到 Program Files 时的数据根；
  2. 安装目录下的 data / plugins / downloads + config.json：装到非系统盘时
     宿主按「便携布局」把数据放在程序目录（见 src/paths.rs 的 detect_home）。
  （注释里不能写花括号占位符，Inno 会把右花括号当成注释结束。）
  只有用户在卸载弹窗里明确选「是」才会调用；任一处删除失败返回 False。 }
function DeleteUserDataAll(): Boolean;
var
  AppDir: String;
begin
  Result := True;
  if DirExists(UserDataDir()) then
    if not DelTree(UserDataDir(), True, True, True) then
      Result := False;

  AppDir := ExpandConstant('{app}');
  if DirExists(AppDir + '\data') then
    if not DelTree(AppDir + '\data', True, True, True) then
      Result := False;
  if DirExists(AppDir + '\plugins') then
    if not DelTree(AppDir + '\plugins', True, True, True) then
      Result := False;
  if DirExists(AppDir + '\downloads') then
    if not DelTree(AppDir + '\downloads', True, True, True) then
      Result := False;
  DeleteFile(AppDir + '\config.json');
end;

{ 删除 SCM 系统服务注册（增量 53）。
  - stop 让服务主循环收敛 aria2c；未运行 / 不存在时 sc 返回 1062 / 1060，忽略。
  - delete 清掉服务注册项，避免重装后 binPath 仍指向旧安装目录。
  必须在删文件前执行：服务进程存活时会持有 aria2-service.exe 的映射。 }
procedure DeleteWindowsService();
var
  ResultCode: Integer;
begin
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop Aria2-Harness-Service', '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode);
  { SCM 的 stop 是异步的，稍等再删，避免撞上 1072「服务已标记为删除」。 }
  Sleep(800);
  Exec(ExpandConstant('{sys}\sc.exe'), 'delete Aria2-Harness-Service', '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode);
end;

{ 覆盖安装开始前调用：返回空串 = 继续安装（Inno 的约定）。
  静默安装同样会走到这里，因此升级脚本 / 企业批量部署也能避免"文件被占用"。 }
function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  NeedsRestart := False;
  KillAppProcesses();
end;

{ 卸载分两阶段：
  - usUninstall：删文件之前。强杀进程 / 删计划任务 / 删自启值，保证文件与注册表
    能被干净移除（哪怕软件还挂在托盘里）。
  - usPostUninstall：文件已删完。此时再问"是否删用户数据"，避免卸载器自己
    与 sqlite / WebView2 缓存抢锁。 }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  DataDir: String;
  Prompt: String;
begin
  if CurUninstallStep = usUninstall then
  begin
    { 主窗口「关闭」只是收进托盘，进程仍在跑；不 /F 杀就删不掉 exe / dll / sqlite。 }
    KillAppProcesses();
    DeleteWindowsService();
    DeleteServiceTask();

    { 开机自启值由程序运行时写入（见 src-host/src/lib.rs 的 set_autostart），
      安装包从未创建过它，所以不能用 [Registry] + uninsdeletevalue —— 那会在
      安装阶段先写一条空值进 Run 键。直接在卸载阶段删值：值不存在时
      RegDeleteValue 静默失败，无副作用。 }
    RegDeleteValue(HKEY_CURRENT_USER, AutostartRunKey, AutostartValueName);
    Exit;
  end;

  if CurUninstallStep <> usPostUninstall then
    Exit;

  { 静默卸载（升级脚本 / 企业批量）必须零交互：默认保留用户数据。 }
  if UninstallSilent then
    Exit;

  DataDir := UserDataDir();
  { 非系统盘安装（便携布局）时真实数据在安装目录的 data 下，同样要给用户选择权。 }
  if not DirExists(DataDir) and not DirExists(ExpandConstant('{app}\data')) then
    Exit;

  Prompt := FmtMessage(CustomMessage('DeleteUserDataPrompt'), [DataDir]);
  if MsgBox(Prompt, mbConfirmation, MB_YESNO) <> IDYES then
    Exit;

  { 进程已在 usUninstall 阶段被杀掉，这里通常一次就能删干净；
    仍保留重试，兜住外部程序（杀软 / 索引器）短暂占用。 }
  if DeleteUserDataAll() then
    Exit;
  Sleep(500);
  if DeleteUserDataAll() then
    Exit;
  Sleep(1000);
  if not DeleteUserDataAll() then
    MsgBox(FmtMessage(CustomMessage('DeleteUserDataFailed'), [DataDir]),
      mbError, MB_OK);
end;
