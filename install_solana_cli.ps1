$ErrorActionPreference = 'Stop'

$version = 'v4.3.0-rc.1'
$tempDir = 'C:\agave-install-tmp'
$installer = Join-Path $tempDir 'agave-install-init.exe'
$downloadUrl = "https://release.anza.xyz/$version/agave-install-init-x86_64-pc-windows-msvc.exe"
$archive = Join-Path $tempDir 'solana-release-windows.tar.bz2'
$userInstallRoot = Join-Path $env:USERPROFILE '.local\share\solana'
$userBin = Join-Path $userInstallRoot 'bin'

New-Item -ItemType Directory -Force -Path $tempDir | Out-Null
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
	[Security.Principal.WindowsBuiltInRole]::Administrator
)

if ($isAdmin) {
	Invoke-WebRequest -Uri $downloadUrl -OutFile $installer
	& $installer $version
	if ($LASTEXITCODE -ne 0) {
		throw "Agave installer failed with exit code $LASTEXITCODE."
	}
} else {
	$archiveUrl = "https://github.com/anza-xyz/agave/releases/download/$version/solana-release-x86_64-pc-windows-msvc.tar.bz2"
	if (-not (Test-Path $archive)) {
		Invoke-WebRequest -Uri $archiveUrl -OutFile $archive
	}
	New-Item -ItemType Directory -Force -Path $userInstallRoot | Out-Null
	$extractRoot = Join-Path $tempDir 'solana-release-extracted'
	if (Test-Path $extractRoot) {
		Remove-Item -Recurse -Force $extractRoot
	}
	New-Item -ItemType Directory -Force -Path $extractRoot | Out-Null
	& tar.exe -xjf $archive -C $extractRoot
	if ($LASTEXITCODE -ne 0) {
		throw "User-level archive extraction failed with exit code $LASTEXITCODE."
	}
	$binSource = Get-ChildItem -Path $extractRoot -Directory -Recurse |
		Where-Object { $_.Name -eq 'bin' } |
		Select-Object -First 1
	if (-not $binSource) {
		throw "User-level archive extraction completed, but no bin directory was found."
	}
	Copy-Item -Path (Join-Path $binSource.FullName '*') -Destination $userBin -Force
	$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
	if ($userPath -notlike "*$userBin*") {
		[Environment]::SetEnvironmentVariable('Path', "$userBin;$userPath", 'User')
	}
	$env:Path = "$userBin;$env:Path"
}

Write-Host 'Installation finished. Open a new terminal, then run:'
Write-Host 'solana --version'
Write-Host 'solana-keygen --version'