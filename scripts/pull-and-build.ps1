[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('release', 'debug')]
    [string]$Profile = 'release'
)

$ErrorActionPreference = 'Stop'

Push-Location -LiteralPath (Split-Path -Parent $PSScriptRoot)
try {
    $cargoCommand = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    $cargo = if ($cargoCommand) {
        $cargoCommand.Source
    } else {
        $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
        Join-Path $cargoHome 'bin/cargo.exe'
    }
    if (!(Test-Path -LiteralPath $cargo -PathType Leaf)) {
        throw 'Cargo was not found. Install Rust with rustup or add Cargo to PATH.'
    }

    git pull --ff-only
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    & $cargo dist ($Profile.ToLowerInvariant())
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
