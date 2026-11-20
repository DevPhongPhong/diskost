$ErrorActionPreference = "Stop"
$mingw = "C:\msys64\mingw64\bin"
if (-not (Get-Command g++ -ErrorAction SilentlyContinue) -and (Test-Path "$mingw\g++.exe")) {
    $env:PATH = "$mingw;$env:PATH"
}
$root = Split-Path -Parent $PSScriptRoot
if (Get-Command g++ -ErrorAction SilentlyContinue) {
    cmake -S "$root\server" -B "$root\server\build" -G "MinGW Makefiles" -DCMAKE_BUILD_TYPE=Release
    cmake --build "$root\server\build"
} else {
    cmake -S "$root\server" -B "$root\server\build"
    cmake --build "$root\server\build" --config Release
}
ctest --test-dir "$root\server\build" -C Release --output-on-failure
