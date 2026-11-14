# Windows version of bump-version.sh: increments the patch version (or minor/major/set X.Y.Z)
# and writes it to VERSION, Cargo.toml and tauri.conf.json. Prints the new version.
param([string]$Part = "patch", [string]$Value = "")
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$cur = (Get-Content "$root\VERSION" -Raw).Trim()
$p = $cur.Split(".") | ForEach-Object { [int]$_ }
switch ($Part) {
  "major" { $new = "$($p[0] + 1).0.0" }
  "minor" { $new = "$($p[0]).$($p[1] + 1).0" }
  "patch" { $new = "$($p[0]).$($p[1]).$($p[2] + 1)" }
  "set"   { $new = $Value }
  default { throw "usage: bump-version.ps1 [patch|minor|major|set X.Y.Z]" }
}
if ($new -notmatch '^\d+\.\d+\.\d+$') { throw "bad version: $new" }
Set-Content "$root\VERSION" $new -NoNewline
$cargo = "$root\src-tauri\Cargo.toml"
$text = Get-Content $cargo -Raw
$rx = [regex]'(?s)(\[package\][^\[]*?\nversion = ")[^"]+(")'
Set-Content $cargo ($rx.Replace($text, "`${1}$new`${2}", 1)) -NoNewline
$conf = "$root\src-tauri\tauri.conf.json"
$text = Get-Content $conf -Raw
Set-Content $conf ([regex]::Replace($text, '("version":\s*")[^"]+(")', "`${1}$new`${2}")) -NoNewline
$lock = "$root\src-tauri\Cargo.lock"
if (Test-Path $lock) {
  $text = Get-Content $lock -Raw
  Set-Content $lock ([regex]::Replace($text, '(\[\[package\]\]\r?\nname = "screen_cast"\r?\nversion = ")[^"]+(")', "`${1}$new`${2}")) -NoNewline
}
Write-Output $new
