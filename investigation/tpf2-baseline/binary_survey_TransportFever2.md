# Binary survey: `TransportFever2.exe`

Static, read-only survey produced by `tools/re/binary_survey.py`. The target was not launched or modified.

## 1. Identity

| field | value |
|---|---|
| path | `E:\SteamLibrary\steamapps\common\Transport Fever 2\TransportFever2.exe` |
| format | PE |
| architecture | x86_64 (64-bit, little-endian) |
| file size | 69.47 MiB (72843280 B) |
| SHA-256 | `782b904a8f7bbdac1f7a18528f1a5c778691e5aa3087c37c351bf6912585175c` |
| SHA-1 | `170970c8b49a57d23dfd061278254645a85af0d5` |
| MD5 | `0109d7f7d165dbb10a3bd8cc256de8f0` |
| image base | 0x140000000 |
| entry point | RVA 0x469A310 (VA 0x14469A310) |
| PE timestamp | 0x675ABCC6 |
| linker version | 14.16 |
| SizeOfImage | 0x46CE000 |
| DLL characteristics | 0x8160 HIGH_ENTROPY_VA, DYNAMIC_BASE/ASLR, NX_COMPAT/DEP |
| PDB path | `C:\GitLab-Runner\builds\1BJoMpBZ\0\ug\urban_games\build\steam\train_fever\TransportFever2.pdb` |
| Authenticode signed | no |
| overlay | none |

Rich header (MSVC toolchain components, `prodid build count`):

```
prodid=147 build=30729 count=22
prodid=257 build=26706 count=4
prodid=199 build=41118 count=1
prodid=261 build=26706 count=40
prodid=260 build=26706 count=10
prodid=259 build=26706 count=14
prodid=260 build=29395 count=1
prodid=260 build=27050 count=74
prodid=257 build=27050 count=6
prodid=257 build=30145 count=2
prodid=257 build=27905 count=2
prodid=257 build=29395 count=16
prodid=257 build=26433 count=3
prodid=261 build=27045 count=41
prodid=257 build=27045 count=4
prodid=1 build=0 count=1097
prodid=261 build=27050 count=1230
prodid=256 build=27050 count=1
prodid=255 build=27050 count=1
prodid=258 build=27050 count=1
```

## 2. Sections

Entropy near 8.0 in an executable or data section is a packing/encryption signal. RWX or high-entropy executable sections are called out below.

| name | RVA | virtual size | raw size | perms | entropy |
|---|---|---|---|---|---|
| `.text` | 0x1000 | 0x2F08308 | 0x2F08400 | r-x | 6.498 |
| `.rdata` | 0x2F0A000 | 0x12288A4 | 0x1228A00 | r-- | 5.741 |
| `.data` | 0x4133000 | 0x34FC48 | 0x1FEC00 | rw- | 4.427 |
| `.pdata` | 0x4483000 | 0x194808 | 0x194A00 | r-- | 7.019 |
| `_RDATA` | 0x4618000 | 0x9850 | 0x9A00 | r-- | 7.173 |
| `.rsrc` | 0x4622000 | 0x4B1B8 | 0x4B200 | r-- | 1.916 |
| `.reloc` | 0x466E000 | 0x2B388 | 0x2B400 | r-- | 5.511 |
| `.bind` | 0x469A000 | 0x33810 | 0x33810 | r-x | 7.953 |

> Note: `.bind` entropy 7.95

## 3. Imports

Game-folder libraries (imported from the image's own directory) are the ones a proxy loader can stand in front of. System libraries are shipped by the OS.

### Game-folder libraries

| library | imported symbols | exports | proxy score |
|---|---|---|---|
| `alut.dll` | 5 | 20 | **candidate** |
| `GFSDK_Aftermath_Lib.x64.dll` | 7 | 39 |  |
| `VCRUNTIME140.dll` | 23 | 71 |  |
| `OpenAL32.dll` | 27 | 176 |  |
| `nvtt.dll` | 25 | 228 |  |
| `SDL2.dll` | 73 | 738 |  |
| `steam_api64.dll` | 10 | 1065 |  |
| `MSVCP140.dll` | 358 | 1515 |  |
| `icuuc61.dll` | 55 | 2810 |  |
| `icuin61.dll` | 119 | 5105 |  |

Best proxy-loader candidate: **`alut.dll`** -- statically imported from the game folder with the fewest exports (20). This is the `alut.dll` role in the TPF2 mods.

### System libraries (19)

- `api-ms-win-crt-convert-l1-1-0.dll` (7): strtod, strtoll, strtol, strtoul, strtoull, _itoa_s, atoi
- `api-ms-win-crt-environment-l1-1-0.dll` (1): getenv
- `api-ms-win-crt-filesystem-l1-1-0.dll` (4): _lock_file, remove, _unlock_file, rename
- `api-ms-win-crt-heap-l1-1-0.dll` (8): _set_new_mode, realloc, malloc, calloc, _callnewh, free, _aligned_free, _aligned_malloc
- `api-ms-win-crt-locale-l1-1-0.dll` (3): setlocale, _configthreadlocale, localeconv
- `api-ms-win-crt-math-l1-1-0.dll` (57): __setusermatherr, exp2, _fpclass, _finite, expf, asinf, fmodf, atan2f ... +49
- `api-ms-win-crt-runtime-l1-1-0.dll` (27): _initialize_onexit_table, _invalid_parameter_noinfo_noreturn, _register_onexit_function, exit, terminate, _set_invalid_parameter_handler, _crt_atexit, _beginthreadex ... +19
- `api-ms-win-crt-stdio-l1-1-0.dll` (39): _pclose, fwrite, __stdio_common_vsnprintf_s, fputc, _ftelli64, _fseeki64, fflush, clearerr ... +31
- `api-ms-win-crt-string-l1-1-0.dll` (22): isdigit, isalnum, toupper, iswspace, strspn, strcoll, tolower, iscntrl ... +14
- `api-ms-win-crt-time-l1-1-0.dll` (12): _get_dstbias, _get_timezone, _localtime64_s, _tzset, _difftime64, clock, _mktime64, strftime ... +4
- `api-ms-win-crt-utility-l1-1-0.dll` (3): rand, srand, qsort
- `KERNEL32.dll` (126): IsProcessorFeaturePresent, TerminateProcess, UnhandledExceptionFilter, RtlVirtualUnwind, RtlLookupFunctionEntry, InitializeSListHead, SleepConditionVariableCS, WakeAllConditionVariable ... +118
- `ole32.dll` (1): CoTaskMemFree
- `OPENGL32.dll` (2): wglGetProcAddress, wglGetCurrentContext
- `PSAPI.DLL` (1): GetProcessMemoryInfo
- `SHELL32.dll` (3): CommandLineToArgvW, ShellExecuteW, SHGetKnownFolderPath
- `USER32.dll` (2): SetClassLongPtrW, LoadIconW
- `WINHTTP.dll` (14): WinHttpQueryDataAvailable, WinHttpAddRequestHeaders, WinHttpReadData, WinHttpOpen, WinHttpSetStatusCallback, WinHttpWriteData, WinHttpSendRequest, WinHttpOpenRequest ... +6
- `WS2_32.dll` (21): WSASocketW, WSASend, freeaddrinfo, __WSAFDIsSet, accept, bind, WSAGetLastError, WSASetLastError ... +13

## 4. Exports

119 exported symbols. First 40:

```
0x02748740  FT_Activate_Size
0x02753640  FT_Add_Default_Modules
0x02748770  FT_Add_Module
0x02748A60  FT_Angle_Diff
0x02748AD0  FT_Atan2
0x02748B50  FT_Attach_File
0x02748B90  FT_Attach_Stream
0x027A4240  FT_Bitmap_Convert
0x027A4730  FT_Bitmap_Copy
0x027A4900  FT_Bitmap_Done
0x027A4960  FT_Bitmap_Embolden
0x027A4CC0  FT_Bitmap_Init
0x027A4CC0  FT_Bitmap_New
0x02748D70  FT_CeilFix
0x02748D80  FT_Cos
0x02748DB0  FT_DivFix
0x02748E20  FT_Done_Face
0x02753690  FT_Done_FreeType
0x02748FA0  FT_Done_Library
0x027490F0  FT_Done_Size
0x02749210  FT_Face_GetCharVariantIndex
0x02749290  FT_Face_GetCharVariantIsDefault
0x027492E0  FT_Face_GetCharsOfVariant
0x02749330  FT_Face_GetVariantSelectors
0x02749370  FT_Face_GetVariantsOfChar
0x027493C0  FT_Face_Properties
0x02749470  FT_FloorFix
0x02749480  FT_Get_Advance
0x02749550  FT_Get_Advances
0x027496D0  FT_Get_CMap_Format
0x02749730  FT_Get_CMap_Language_ID
0x02749790  FT_Get_Char_Index
0x027497E0  FT_Get_Charmap_Index
0x02749820  FT_Get_First_Char
0x027498C0  FT_Get_Font_Format
0x02749900  FT_Get_Glyph_Name
0x027499E0  FT_Get_Kerning
0x02749BA0  FT_Get_Module
0x02749C50  FT_Get_Name_Index
0x02749CF0  FT_Get_Next_Char
```

The export set names the statically-linked libraries folded into the image (e.g. FreeType `FT_*` here), which is a fingerprint of the build.

## 5. TLS callbacks

Code that runs before the entry point (a proxy DLL must expect it):

- RVA 0x2708A10 (VA 0x142708A10)
- RVA 0x2BF4170 (VA 0x142BF4170)
- RVA 0x2BF4034 (VA 0x142BF4034)

## 6. Packer and anti-tamper indicators

Section-name matches:

- section `.bind` -> Steam DRM (SteamStub)

SteamStub (`.bind`) header decode:

```
variant                    v3.x (header 0xF0)
signature                  0xC0DEC0DF
original_entry_point_rva   0x2BF4BC8
steam_app_id               1066780
flags                      0x6
```

> SteamStub decrypts the real code at load time behind the entry point. A static tool sees only the stub, so signature scanning of the game's own functions must run against the in-memory image (or a Steamless-style dumped image), not the on-disk file. This is the TPF2 situation and does not by itself block a proxy-DLL hook, which loads into the unpacked process.

## 7. RTTI

- MSVC RTTI type descriptors (`.?AV`/`.?AU...@@`): **4136**
- Itanium type-info names (`_ZTS...`): **0**

Present RTTI gives an independent naming axis (vftables -> class + slot), as in the TPF2 pipeline. Samples:

```
.?AVruntime_error@std@@
.?AVexception@std@@
.?AVexception@boost@@
.?AVbad_cast@std@@
.?AUException@@
.?AUFileSystemException@file_system@@
```

## 8. Embedded Lua

- `Lua 5.2.2`
- `Lua 5.2`
- `$LuaVersion: Lua 5.2.2  Copyright (C) 1994-2013 Lua.org, PUC-Rio $`
- `$LuaAuthors: R. Ierusalimschy, L. H. de Figueiredo, W. Celes $`

The Lua version fixes the script sandbox the probe mods assume (docs/DAY_ONE.md section 3). TPF2 is Lua 5.2.2 with sol2 bindings.

## 9. Compiler assert strings (naming feedstock)

These are what `name_functions.py` turns into a symbol map.

| kind | count |
|---|---|
| MSVC `__FUNCSIG__` signatures | 18897 |
| Clang/GCC `__PRETTY_FUNCTION__` signatures | 0 |
| `__FILE__` source paths | 729 |

Common source-path prefix: `c:\gitlab-runner\builds\1bjompbz\0\ug\urban_games\train_fever\src\`

## 10. TPF2-era engine symbols

Whether TPF3 shares the TPF2 command pipeline and naming (docs/ARCHITECTURE.md open questions).

| token | occurrences |
|---|---|
| `make_cmd::` | 19 |
| `CommandList::Add` | 1 |
| `CommandList::` | 2 |
| `GameSim::Step` | 1 |
| `CGame::RunGameSimLoop` | 1 |
| `CGame::` | 8 |
| `CGameTime::` | 7 |
| `api.cmd` | 0 |
| `sendCommand` | 1 |
| `buildProposal` | 1 |
| `TransportVehicleConfig` | 174 |
| `ecs::Engine` | 863 |

