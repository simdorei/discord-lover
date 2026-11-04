[CmdletBinding()]
param([string]$TargetDirectory, [switch]$DebugBuild)
$ErrorActionPreference='Stop'
$root=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Import-Module (Join-Path $PSScriptRoot 'CdrBuildProvenance.psm1') -Force
if (-not $TargetDirectory) { $TargetDirectory=if($env:CARGO_TARGET_DIR){$env:CARGO_TARGET_DIR}else{Join-Path $root 'target'} }
if (-not [IO.Path]::IsPathRooted($TargetDirectory)) { $TargetDirectory=Join-Path $root $TargetDirectory }
$arguments=@('build','--locked','-p','cdr-runtime','--bin','cdr-runtime','--target-dir',$TargetDirectory)
$profile=if($DebugBuild){'debug'}else{'release'}
if(-not $DebugBuild){$arguments+='--release'}
$binary=Join-Path $TargetDirectory "$profile/cdr-runtime.exe"
$source=Invoke-CdrRuntimeBuild -Root $root -BinaryPath $binary -CargoArguments $arguments
Write-Output "verified_binary=$binary source=$($source.Input) sha256=$(Get-CdrBuildFileHash $binary)"
